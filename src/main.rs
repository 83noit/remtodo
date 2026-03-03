//! `remtodo` — bi-directional sync daemon between Apple Reminders and a todo.txt file.
//!
//! # Subcommands
//! - `sync` — run a single sync pass (or loop with `--daemon`); honours `--dry-run`
//! - `restore` — roll back the last sync using the pre-sync undo log
//! - `install` / `uninstall` — manage the launchd agent
//! - `status` — print the agent status and recent log lines
//!
//! # Sync cycle (one pass)
//! 1. Acquire exclusive lock (`SyncLock`) — aborts if another instance is running
//! 2. Load config, state, and create a pre-sync backup
//! 3. For each configured list: fetch reminders, load tasks, compute actions
//!    (`compute_sync_actions_ext`), execute them (`execute_reminder_actions`),
//!    apply safety guards, and collect undo entries
//! 4. Recurrence pass — re-spawn tasks with `rec:` tags up to `MAX_RECURRENCE_PASSES` times
//! 5. Atomically write todo.txt and save state; verify post-sync hash
//!
//! # Signal handling
//! SIGINT / SIGTERM set an `AtomicBool` shutdown flag.  Long-running loops check
//! the flag between iterations and perform a clean exit with per-list rollback.
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{Local, NaiveDateTime};
use log::{debug, error, info, warn};
use todo_lib::todotxt::Task;

use remtodo::config::resolve_config_path;
use remtodo::config::{expand_tilde, AppConfig};
use remtodo::error::SyncError;
use remtodo::lock::SyncLock;
use remtodo::reminder::Reminder;
use remtodo::swift_cli::{BatchItemResult, BatchOp, CreateReminderInput, SwiftCli};
use remtodo::sync::actions::SyncAction;
use remtodo::sync::config::{ListSyncConfig, PriorityMap};
use remtodo::sync::engine::{
    apply_task_actions, build_initial_state, compute_release_set, compute_sync_actions_ext,
    extract_title, synced_field_hash, task_completion_date, task_due_date, task_line_hash,
    task_notes, task_priority, verify_post_sync,
};
use remtodo::sync::persistence::{file_mtime_utc, load_state, resolve_state_path, save_state};
use remtodo::sync::recurrence::collect_recurrence_spawns;
use remtodo::sync::safety::{
    check_bulk_deletion, check_first_sync_no_deletions, check_task_count_coherence,
};
use remtodo::sync::state::{SyncItemState, SyncState, SyncedFieldState};
use remtodo::undo::{create_pre_sync_backup, save_undo_log, UndoEntry, UndoLog};

/// Maximum number of recurrence-spawn passes performed after each sync cycle.
///
/// After tasks with `rec:` tags are completed, TTDL spawns successor tasks.
/// This loop re-runs up to this many times so that rapid successive completions
/// (e.g. completing a task on two devices between syncs) are all resolved in one pass.
const MAX_RECURRENCE_PASSES: usize = 3;

/// Apply a successful `CreateReminder` result to `tasks`, `state`, and `undo_entries`.
#[allow(clippy::too_many_arguments)]
fn apply_create_result(
    reminder: &Reminder,
    original_task_line: &str,
    target_list: &str,
    tasks: &mut [Task],
    state: &mut SyncState,
    priority_map: &PriorityMap,
    now: NaiveDateTime,
    undo_entries: &mut Vec<UndoEntry>,
) {
    for t in tasks.iter_mut() {
        if format!("{t}") == original_task_line {
            t.update_tag_with_value("eid", &reminder.external_id);
            let fields = SyncedFieldState {
                title: extract_title(t),
                priority: task_priority(t, priority_map),
                is_completed: t.finished,
                completion_date: task_completion_date(t),
                due_date: task_due_date(t),
                notes: task_notes(t),
                list: target_list.to_owned(),
            };
            let r_hash = synced_field_hash(&fields);
            let item = SyncItemState {
                eid: reminder.external_id.clone(),
                fields,
                reminders_last_modified: None,
                task_line_hash: task_line_hash(t),
                reminders_field_hash: r_hash,
                last_synced: now,
                pushed: true, // push-origin: task → reminder
            };
            state.items.insert(reminder.external_id.clone(), item);
            break;
        }
    }
    undo_entries.push(UndoEntry::UndoCreate {
        eid: reminder.external_id.clone(),
        list_name: target_list.to_owned(),
    });
    info!("Created reminder eid:{}", reminder.external_id);
}

/// Apply a successful `ResurrectReminder` result to `tasks`, `state`, and `undo_entries`.
#[allow(clippy::too_many_arguments)]
fn apply_resurrect_result(
    reminder: &Reminder,
    old_eid: &str,
    target_list: &str,
    tasks: &mut [Task],
    state: &mut SyncState,
    priority_map: &PriorityMap,
    now: NaiveDateTime,
    undo_entries: &mut Vec<UndoEntry>,
) {
    for t in tasks.iter_mut() {
        if t.tags.get("eid").map(|e| e.as_str()) == Some(old_eid) {
            t.update_tag_with_value("eid", &reminder.external_id);
            break;
        }
    }
    // Preserve pushed flag from the old state entry before removing it.
    let old_pushed = state.items.get(old_eid).map(|i| i.pushed).unwrap_or(true);
    state.items.remove(old_eid);
    if let Some(t) = tasks
        .iter()
        .find(|t| t.tags.get("eid").map(|e| e.as_str()) == Some(reminder.external_id.as_str()))
    {
        let fields = SyncedFieldState {
            title: extract_title(t),
            priority: task_priority(t, priority_map),
            is_completed: t.finished,
            completion_date: task_completion_date(t),
            due_date: task_due_date(t),
            notes: task_notes(t),
            list: target_list.to_owned(),
        };
        let r_hash = synced_field_hash(&fields);
        let item = SyncItemState {
            eid: reminder.external_id.clone(),
            fields,
            reminders_last_modified: None,
            task_line_hash: task_line_hash(t),
            reminders_field_hash: r_hash,
            last_synced: now,
            pushed: old_pushed, // preserve origin from resurrected item
        };
        state.items.insert(reminder.external_id.clone(), item);
    }
    undo_entries.push(UndoEntry::UndoCreate {
        eid: reminder.external_id.clone(),
        list_name: target_list.to_owned(),
    });
    info!(
        "Resurrected reminder old_eid:{old_eid} new_eid:{}",
        reminder.external_id
    );
}

/// Context tag kept alongside each `BatchOp` so results can be post-processed
/// without re-matching on the original `SyncAction` slice.
enum BatchedActionKind {
    Create {
        original_task_line: String,
        target_list: String,
    },
    Update {
        eid: String,
    },
    Delete {
        eid: String,
    },
    Resurrect {
        old_eid: String,
        target_list: String,
    },
}

/// Process a parallel `(BatchedActionKind, BatchItemResult)` pair, updating
/// `tasks`, `state`, and `undo_entries` in place.
#[allow(clippy::too_many_arguments)]
fn apply_batch_result(
    kind: &BatchedActionKind,
    result: &BatchItemResult,
    tasks: &mut [Task],
    state: &mut SyncState,
    priority_map: &PriorityMap,
    now: NaiveDateTime,
    reminders: &[Reminder],
    undo_entries: &mut Vec<UndoEntry>,
) {
    if !result.ok {
        let err = result.error.as_deref().unwrap_or("unknown error");
        match kind {
            BatchedActionKind::Create {
                original_task_line, ..
            } => {
                error!("CreateReminder '{}': {err}", original_task_line)
            }
            BatchedActionKind::Update { eid } => error!("UpdateReminder eid:{eid}: {err}"),
            BatchedActionKind::Delete { eid } => error!("DeleteReminder eid:{eid}: {err}"),
            BatchedActionKind::Resurrect { old_eid, .. } => {
                error!("ResurrectReminder eid:{old_eid}: {err}")
            }
        }
        return;
    }

    match kind {
        BatchedActionKind::Create {
            original_task_line,
            target_list,
        } => {
            if let Some(reminder) = &result.reminder {
                apply_create_result(
                    reminder,
                    original_task_line,
                    target_list,
                    tasks,
                    state,
                    priority_map,
                    now,
                    undo_entries,
                );
            }
        }
        BatchedActionKind::Update { eid } => {
            if let Some(old) = reminders.iter().find(|r| r.external_id == *eid) {
                undo_entries.push(UndoEntry::UndoUpdate {
                    old_reminder: old.clone(),
                });
            }
            info!("Updated reminder eid:{eid}");
        }
        BatchedActionKind::Delete { eid } => {
            if let Some(old) = reminders.iter().find(|r| r.external_id == *eid) {
                undo_entries.push(UndoEntry::UndoDelete {
                    reminder: old.clone(),
                });
            }
            info!("Deleted reminder eid:{eid}");
        }
        BatchedActionKind::Resurrect {
            old_eid,
            target_list,
        } => {
            if let Some(reminder) = &result.reminder {
                apply_resurrect_result(
                    reminder,
                    old_eid,
                    target_list,
                    tasks,
                    state,
                    priority_map,
                    now,
                    undo_entries,
                );
            }
        }
    }
}

/// Execute all reminder-side `SyncAction`s by calling the Swift CLI in a single batch.
///
/// Returns `(completed, failures)` where `completed` is `false` if a shutdown signal
/// was received mid-flight.  `failures` counts individual operations whose
/// `BatchItemResult::ok` was `false`; the caller uses this to decide whether to
/// abort the final write.  Task-side actions (e.g. `UpdateTask`) are applied
/// in-memory here via `apply_task_actions` without touching the file.
#[allow(clippy::too_many_arguments)]
fn execute_reminder_actions(
    cli: &SwiftCli,
    actions: &[SyncAction],
    tasks: &mut [Task],
    state: &mut SyncState,
    config: &ListSyncConfig,
    now: NaiveDateTime,
    reminders: &[Reminder],
    undo_entries: &mut Vec<UndoEntry>,
    shutdown: &AtomicBool,
) -> (bool, usize) {
    let priority_map: PriorityMap = config.compiled_priority_map();

    // Handle non-CLI actions first (no subprocess involved).
    for action in actions {
        if let SyncAction::MergeConflict { eid, .. } = action {
            warn!("MergeConflict (reminder side) eid:{eid}");
        }
    }

    // Collect reminder-side CLI operations into a single batch.
    let mut ops: Vec<BatchOp> = Vec::new();
    let mut kinds: Vec<BatchedActionKind> = Vec::new();

    for action in actions {
        if shutdown.load(Ordering::Relaxed) {
            return (false, 0);
        }
        match action {
            SyncAction::CreateReminder { task, target_list } => {
                ops.push(BatchOp::CreateReminder(CreateReminderInput {
                    title: extract_title(task),
                    list_name: target_list.clone(),
                    priority: task_priority(task, &priority_map),
                    due_date: task_due_date(task),
                    notes: task_notes(task),
                    is_completed: task.finished,
                    completion_date: task_completion_date(task),
                }));
                kinds.push(BatchedActionKind::Create {
                    original_task_line: format!("{task}"),
                    target_list: target_list.clone(),
                });
            }
            SyncAction::UpdateReminder {
                eid,
                updated_reminder,
            } => {
                ops.push(BatchOp::UpdateReminder(updated_reminder.clone()));
                kinds.push(BatchedActionKind::Update { eid: eid.clone() });
            }
            SyncAction::DeleteReminder { eid } => {
                ops.push(BatchOp::DeleteReminder {
                    eid: eid.clone(),
                    list_name: config.reminders_list.clone(),
                });
                kinds.push(BatchedActionKind::Delete { eid: eid.clone() });
            }
            SyncAction::ResurrectReminder {
                eid,
                reminder_update,
                target_list,
            } => {
                ops.push(BatchOp::CreateReminder(CreateReminderInput {
                    title: reminder_update.title.clone().unwrap_or_default(),
                    list_name: target_list.clone(),
                    priority: reminder_update.priority.unwrap_or(0),
                    due_date: reminder_update.due_date.clone().and_then(|x| x),
                    notes: reminder_update.notes.clone().and_then(|x| x),
                    is_completed: reminder_update.is_completed.unwrap_or(false),
                    completion_date: reminder_update.completion_date.clone().and_then(|x| x),
                }));
                kinds.push(BatchedActionKind::Resurrect {
                    old_eid: eid.clone(),
                    target_list: target_list.clone(),
                });
            }
            SyncAction::RelinkEid { old_eid, new_eid } => {
                // No Reminders-side I/O — purely local bookkeeping handled by apply_task_actions.
                info!("Relinked EID: {old_eid} → {new_eid}");
            }
            _ => {} // MergeConflict handled above; task-only actions handled elsewhere.
        }
    }

    if ops.is_empty() {
        return (true, 0);
    }

    // Execute all operations in a single Swift process spawn.
    // Fall back to individual calls if the batch subcommand is unavailable
    // (e.g. version skew where the installed binary predates this change).
    let results = match cli.batch(&ops) {
        Ok(r) => r,
        Err(e) => {
            warn!("Batch call failed ({e}), falling back to individual operations");
            return execute_reminder_actions_individual(
                cli,
                actions,
                tasks,
                state,
                config,
                now,
                reminders,
                undo_entries,
                shutdown,
                &priority_map,
            );
        }
    };

    let failures: usize = results.iter().filter(|r| !r.ok).count();
    for (kind, result) in kinds.iter().zip(results.iter()) {
        apply_batch_result(
            kind,
            result,
            tasks,
            state,
            &priority_map,
            now,
            reminders,
            undo_entries,
        );
    }

    (true, failures)
}

/// Individual-call fallback used when the batch subcommand is unavailable.
/// Identical behaviour to the pre-batch implementation.
#[allow(clippy::too_many_arguments)]
fn execute_reminder_actions_individual(
    cli: &SwiftCli,
    actions: &[SyncAction],
    tasks: &mut [Task],
    state: &mut SyncState,
    config: &ListSyncConfig,
    now: NaiveDateTime,
    reminders: &[Reminder],
    undo_entries: &mut Vec<UndoEntry>,
    shutdown: &AtomicBool,
    priority_map: &PriorityMap,
) -> (bool, usize) {
    let mut failures: usize = 0;
    for action in actions {
        if shutdown.load(Ordering::Relaxed) {
            return (false, failures);
        }
        match action {
            SyncAction::CreateReminder { task, target_list } => {
                let input = CreateReminderInput {
                    title: extract_title(task),
                    list_name: target_list.clone(),
                    priority: task_priority(task, priority_map),
                    due_date: task_due_date(task),
                    notes: task_notes(task),
                    is_completed: task.finished,
                    completion_date: task_completion_date(task),
                };
                match cli.create_reminder(&input) {
                    Ok(reminder) => apply_create_result(
                        &reminder,
                        &format!("{task}"),
                        target_list,
                        tasks,
                        state,
                        priority_map,
                        now,
                        undo_entries,
                    ),
                    Err(e) => {
                        failures += 1;
                        error!("CreateReminder '{}': {e}", task.subject);
                    }
                }
            }

            SyncAction::UpdateReminder {
                eid,
                updated_reminder,
            } => match cli.update_reminder(updated_reminder) {
                Ok(_) => {
                    if let Some(old) = reminders.iter().find(|r| r.external_id == *eid) {
                        undo_entries.push(UndoEntry::UndoUpdate {
                            old_reminder: old.clone(),
                        });
                    }
                    info!("Updated reminder eid:{eid}");
                }
                Err(e) => {
                    failures += 1;
                    error!("UpdateReminder eid:{eid}: {e}");
                }
            },

            SyncAction::DeleteReminder { eid } => {
                match cli.delete_reminder(eid, &config.reminders_list) {
                    Ok(()) => {
                        if let Some(old) = reminders.iter().find(|r| r.external_id == *eid) {
                            undo_entries.push(UndoEntry::UndoDelete {
                                reminder: old.clone(),
                            });
                        }
                        info!("Deleted reminder eid:{eid}");
                    }
                    Err(e) => {
                        failures += 1;
                        error!("DeleteReminder eid:{eid}: {e}");
                    }
                }
            }

            SyncAction::ResurrectReminder {
                eid,
                reminder_update,
                target_list,
            } => {
                let input = CreateReminderInput {
                    title: reminder_update.title.clone().unwrap_or_default(),
                    list_name: target_list.clone(),
                    priority: reminder_update.priority.unwrap_or(0),
                    due_date: reminder_update.due_date.clone().and_then(|x| x),
                    notes: reminder_update.notes.clone().and_then(|x| x),
                    is_completed: reminder_update.is_completed.unwrap_or(false),
                    completion_date: reminder_update.completion_date.clone().and_then(|x| x),
                };
                match cli.create_reminder(&input) {
                    Ok(reminder) => apply_resurrect_result(
                        &reminder,
                        eid,
                        target_list,
                        tasks,
                        state,
                        priority_map,
                        now,
                        undo_entries,
                    ),
                    Err(e) => {
                        failures += 1;
                        error!("ResurrectReminder eid:{eid}: {e}");
                    }
                }
            }

            SyncAction::MergeConflict { eid, .. } => {
                warn!("MergeConflict (reminder side) eid:{eid}");
            }

            SyncAction::RelinkEid { old_eid, new_eid } => {
                // No Reminders-side I/O — purely local bookkeeping handled by apply_task_actions.
                info!("Relinked EID: {old_eid} → {new_eid}");
            }

            _ => {}
        }
    }
    (true, failures)
}

/// Run one complete sync pass across all configured lists.
///
/// # Phases
/// 1. **Lock** — acquire `SyncLock`; return early if already held.
/// 2. **Load** — read config, state, task file; create pre-sync backup.
/// 3. **Per-list loop** — for each `ListSyncConfig`:
///    - fetch reminders via Swift CLI
///    - compute actions with `compute_sync_actions_ext`
///    - execute actions (safety guards abort on bulk-delete threshold)
///    - accumulate undo entries
/// 4. **Recurrence** — re-spawn `rec:`-tagged tasks up to `MAX_RECURRENCE_PASSES` times.
/// 5. **Write** — atomically write todo.txt; save state; verify post-sync hash.
///
/// In `dry_run` mode phases 3–5 log what *would* happen but make no changes.
fn sync_once(
    app_config: &AppConfig,
    dry_run: bool,
    shutdown: &AtomicBool,
) -> Result<(), SyncError> {
    if dry_run {
        info!("DRY RUN — no files or reminders will be modified");
    }

    // Resolve state directory first so we can place the lock file there.
    let state_path = resolve_state_path()?;
    let state_dir = state_path.parent().ok_or_else(|| {
        SyncError::Config("Cannot determine state directory from state path".to_string())
    })?;

    // Acquire an exclusive sync lock for the duration of this sync.
    // This prevents two concurrent `remtodo sync` invocations (e.g. launchd
    // firing while a manual sync is already running) from corrupting state.
    let _lock = SyncLock::acquire(state_dir)?;

    let cli = SwiftCli::new()?;
    let output_path = expand_tilde(&app_config.output);
    let output = Path::new(&output_path);

    let tasks = if output.exists() {
        todo_lib::todo::load(output)?
    } else {
        Vec::new()
    };

    let sync_timestamp = Local::now().to_rfc3339();
    let now = Local::now().naive_utc();
    let task_mtime = file_mtime_utc(output);

    let mut state_opt = load_state(&state_path)?;

    // First run: build combined initial state from all configured lists.
    let is_first_sync = state_opt.is_none();
    let mut reconciled_pairs: Vec<(String, usize)> = Vec::new();
    if is_first_sync {
        let mut all_reminders = Vec::new();
        for lc in &app_config.lists {
            let reminders = cli.get_reminders(&lc.reminders_list, app_config.include_completed)?;
            all_reminders.extend(reminders);
        }
        let (initial_state, pairs) = build_initial_state(&all_reminders, &tasks, now);
        state_opt = Some(initial_state);
        reconciled_pairs = pairs;
    }

    let mut current_tasks = tasks;
    // Stamp EIDs onto tasks matched by title+due during bootstrap reconciliation.
    for (eid, idx) in &reconciled_pairs {
        current_tasks[*idx].update_tag_with_value("eid", eid);
        info!(
            "Bootstrap: stamped eid:{} on task '{}'",
            eid,
            extract_title(&current_tasks[*idx])
        );
    }
    let mut current_state = state_opt.unwrap();

    if !dry_run {
        create_pre_sync_backup(output, &state_path, state_dir)?;
    }
    let mut undo_entries: Vec<UndoEntry> = Vec::new();

    // Tracks how many per-list sync iterations completed without interruption.
    // Used in the Interrupted error message so the user knows what was saved.
    let mut completed_lists: usize = 0;

    // Outer loop: re-runs the full multi-list sync pass when recurring tasks are
    // completed, so that their next instances are picked up and pushed as new
    // reminders within the same user-initiated sync.
    for pass in 1..=MAX_RECURRENCE_PASSES {
        if shutdown.load(Ordering::Relaxed) {
            warn!("Shutdown requested — skipping recurrence pass {pass}");
            break;
        }
        let mut pass_recurrence_spawns: Vec<Task> = Vec::new();

        // Cross-list deduplication: reset each pass so newly spawned tasks can
        // be claimed by the appropriate list on the re-run.
        let mut claimed_create_hashes: std::collections::HashSet<u64> =
            std::collections::HashSet::new();

        // Compute release set once per pass, across all lists.
        // This identifies tasks that should be released from sync under
        // sticky_tracking = Auto (task changed + no longer admitted).
        let today = chrono::Local::now().date_naive();
        let release_eids =
            compute_release_set(&current_tasks, &current_state, &app_config.lists, today);
        if !release_eids.is_empty() {
            debug!("Release set this pass: {:?}", release_eids);
        }

        for lc in &app_config.lists {
            if shutdown.load(Ordering::Relaxed) {
                warn!("Shutdown requested — saving after {completed_lists} completed list(s)");
                break;
            }

            // Always include completed reminders so the engine can detect completions
            // made on another device (via Case A / three_way_diff) rather than
            // triggering a spurious ResurrectReminder.  The per-list
            // sync_initial_completed flag still controls whether newly-seen completed
            // reminders with no state entry are imported as tasks.
            let reminders = cli.get_reminders(&lc.reminders_list, true)?;
            info!(
                "Fetched {} reminders from '{}'",
                reminders.len(),
                lc.reminders_list
            );
            let actions = {
                let raw = compute_sync_actions_ext(
                    &reminders,
                    &current_tasks,
                    &current_state,
                    lc,
                    now,
                    task_mtime,
                    &release_eids,
                    app_config.timestamp_tolerance_secs,
                );
                // Filter out CreateReminder actions for tasks already claimed by a
                // previous list. Log a warning so the user knows to fix their config.
                let mut filtered = Vec::with_capacity(raw.len());
                for action in raw {
                    if let SyncAction::CreateReminder { task, .. } = &action {
                        let h = task_line_hash(task);
                        if claimed_create_hashes.contains(&h) {
                            warn!(
                                "Task '{}' matches push_filter for list '{}' but was already \
                                 claimed by a previous list — skipping. \
                                 Check your push_filter config for overlapping rules.",
                                task.subject, lc.reminders_list
                            );
                            continue;
                        }
                    }
                    filtered.push(action);
                }
                // Claim all remaining CreateReminder targets for this list.
                for action in &filtered {
                    if let SyncAction::CreateReminder { task, .. } = action {
                        claimed_create_hashes.insert(task_line_hash(task));
                    }
                }
                filtered
            };

            // Safety invariant: on the very first sync both sides can only grow.
            // Any deletion action here means state is corrupt or there is a bug —
            // abort before touching either file.
            if is_first_sync {
                check_first_sync_no_deletions(&actions, &lc.reminders_list, &state_path)?;
            }

            // Safety invariant: bulk reminder deletion guard.
            // Deleting more than half the tracked reminders for a list in a single sync
            // is almost certainly a bug or data corruption, not intentional user action.
            {
                let tracked_for_list = current_state
                    .items
                    .values()
                    .filter(|item| item.fields.list == lc.reminders_list)
                    .count();
                check_bulk_deletion(
                    &actions,
                    tracked_for_list,
                    &lc.reminders_list,
                    &state_path,
                    app_config.max_delete_percent,
                )?;
            }

            // Collect recurrence spawns before applying actions (current_tasks still
            // holds the pre-completion state needed by collect_recurrence_spawns).
            let spawns = collect_recurrence_spawns(&actions, &current_tasks);
            pass_recurrence_spawns.extend(spawns);

            let pre_task_count = current_tasks.len();
            let delete_task_count = actions
                .iter()
                .filter(|a| matches!(a, SyncAction::DeleteTask { .. }))
                .count();

            // Save a rollback snapshot before applying actions so that, if we
            // are interrupted mid-list, we can discard the partial Reminders
            // mutations and write only consistent (fully-applied) state.
            let pre_list_tasks = current_tasks.clone();
            let pre_list_state = current_state.clone();

            let (mut new_tasks, mut new_state) =
                apply_task_actions(&actions, current_tasks, &current_state, lc, now);

            // Safety invariant: task count coherence.
            // After applying actions, the surviving task count plus the explicitly deleted tasks
            // must equal or exceed the pre-sync count. A shortfall means tasks were silently dropped.
            check_task_count_coherence(
                pre_task_count,
                new_tasks.len(),
                delete_task_count,
                &lc.reminders_list,
            )?;

            if dry_run {
                for action in &actions {
                    let desc = match action {
                        SyncAction::CreateTask { eid, .. } => format!("CreateTask eid:{eid}"),
                        SyncAction::CreateReminder { task, target_list } => {
                            format!("CreateReminder '{}' → {target_list}", task.subject)
                        }
                        SyncAction::UpdateTask { eid, .. } => format!("UpdateTask eid:{eid}"),
                        SyncAction::UpdateReminder { eid, .. } => {
                            format!("UpdateReminder eid:{eid}")
                        }
                        SyncAction::DeleteTask { eid } => format!("DeleteTask eid:{eid}"),
                        SyncAction::DeleteReminder { eid } => format!("DeleteReminder eid:{eid}"),
                        SyncAction::MergeConflict { eid, .. } => {
                            format!("MergeConflict eid:{eid}")
                        }
                        SyncAction::ResurrectTask { eid, .. } => {
                            format!("ResurrectTask eid:{eid}")
                        }
                        SyncAction::ResurrectReminder { eid, .. } => {
                            format!("ResurrectReminder eid:{eid}")
                        }
                        SyncAction::CleanSentinelTag { sentinel_eid } => {
                            format!("CleanSentinelTag {sentinel_eid} → na")
                        }
                        SyncAction::RelinkEid { old_eid, new_eid } => {
                            format!("RelinkEid {old_eid} → {new_eid}")
                        }
                    };
                    info!("[dry-run] {desc}");
                }
            } else {
                let (complete, failures) = execute_reminder_actions(
                    &cli,
                    &actions,
                    &mut new_tasks,
                    &mut new_state,
                    lc,
                    now,
                    &reminders,
                    &mut undo_entries,
                    shutdown,
                );
                if failures > 0 {
                    warn!(
                        "{failures} reminder action(s) failed for list '{}' — see error log above",
                        lc.reminders_list
                    );
                }
                if !complete {
                    // Interrupted mid-list: roll back to the pre-list snapshot so
                    // only fully-applied lists are written to disk. Some reminder
                    // mutations for this list may already have been sent to
                    // Reminders — they will be re-processed on the next sync run
                    // (bootstrap reconciliation handles re-imports cleanly).
                    warn!(
                        "Shutdown requested mid-list '{}' — rolling back to \
                         pre-list state ({completed_lists} list(s) will be saved)",
                        lc.reminders_list
                    );
                    current_tasks = pre_list_tasks;
                    current_state = pre_list_state;
                    break;
                }
            }
            current_tasks = new_tasks;
            current_state = new_state;
            completed_lists += 1;
        }

        // If no recurring tasks were completed this pass, we're done.
        if pass_recurrence_spawns.is_empty() {
            break;
        }

        if dry_run {
            info!(
                "[dry-run] Pass {pass}: would spawn {} recurring task(s) and re-sync",
                pass_recurrence_spawns.len()
            );
            break;
        }

        if pass == MAX_RECURRENCE_PASSES {
            warn!(
                "Recurrence pass limit ({MAX_RECURRENCE_PASSES}) reached — \
                 spawned tasks will be synced on the next run"
            );
            current_tasks.extend(pass_recurrence_spawns);
            break;
        }

        info!(
            "Pass {pass}: spawned {} recurring task(s); running follow-up sync pass",
            pass_recurrence_spawns.len()
        );
        current_tasks.extend(pass_recurrence_spawns);
        // Continue outer loop for follow-up pass.
    }

    if dry_run {
        info!(
            "[dry-run] Would write {} tasks to {}",
            current_tasks.len(),
            output_path
        );
        info!("[dry-run] Would save state to {}", state_path.display());
    } else {
        // Safety: verify todo.txt was not modified externally while we were
        // syncing. If it was, our in-memory task list is stale and writing
        // it would silently overwrite the user's edits.
        let current_mtime = file_mtime_utc(output);
        if current_mtime != task_mtime {
            return Err(SyncError::SafetyAbort(format!(
                "{} was modified externally during sync (mtime changed from {:?} to {:?}). \
                 Re-run sync to pick up the latest version.",
                output_path, task_mtime, current_mtime,
            )));
        }

        // Post-sync consistency check: surface engine bugs before writing.
        // Reports warnings but does not abort — state is already in the best
        // shape we have and writing it is safer than leaving the old version.
        let issues = verify_post_sync(&current_tasks, &current_state);
        if !issues.is_empty() {
            warn!(
                "Post-sync verification found {} issue(s) — please report this as a bug:",
                issues.len()
            );
            for issue in &issues {
                warn!("  {issue}");
            }
        } else {
            debug!("Post-sync verification passed");
        }

        // todo_lib::todo::save writes via .todo.tmp → rename(2), so a kill
        // between the write and the rename cannot leave a partially-written file.
        todo_lib::todo::save(&current_tasks, output)?;
        info!("Wrote {} tasks to {}", current_tasks.len(), output_path);

        save_state(&state_path, &current_state)?;
        debug!("State saved to {}", state_path.display());

        let undo_log = UndoLog {
            timestamp: sync_timestamp,
            todo_original_path: output_path.clone(),
            entries: undo_entries,
        };
        save_undo_log(state_dir, &undo_log)?;
        debug!("Undo log saved to {}", state_dir.display());
    }

    // Return a distinct error code if we were interrupted — state has already
    // been saved for completed lists, so the user can re-run to finish.
    if shutdown.load(Ordering::Relaxed) {
        return Err(SyncError::Interrupted(completed_lists));
    }

    Ok(())
}

fn load_or_default_config(config_path_opt: Option<&str>) -> Result<AppConfig, SyncError> {
    match config_path_opt {
        Some(path) => remtodo::config::load_config(Path::new(path)),
        None => {
            let default_path = resolve_config_path();
            if default_path.exists() {
                remtodo::config::load_config(&default_path)
            } else {
                Ok(AppConfig {
                    output: "todo.txt".to_string(),
                    include_completed: false,
                    poll_interval_secs: 60,
                    max_delete_percent: 50,
                    timestamp_tolerance_secs: 0,
                    lists: vec![ListSyncConfig::new("Tasks")],
                })
            }
        }
    }
}

struct SyncArgs {
    config_path: Option<String>,
    dry_run: bool,
}

fn parse_sync_args(args: &[String]) -> SyncArgs {
    let mut config_path = None;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => dry_run = true,
            "--config" => {
                i += 1;
                config_path = Some(args.get(i).cloned().unwrap_or_else(|| {
                    eprintln!("--config requires a value");
                    std::process::exit(1);
                }));
            }
            other => {
                eprintln!("Unknown option: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    SyncArgs {
        config_path,
        dry_run,
    }
}

fn run() -> Result<(), SyncError> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!(
            "Usage: remtodo <sync|install|uninstall|status|restore> [--config <path>] [--dry-run]"
        );
        std::process::exit(1);
    }

    let subcommand = args[1].as_str();

    // Handle --version / -V before anything else (no logging, no config needed).
    if subcommand == "--version" || subcommand == "-V" || subcommand == "version" {
        println!("remtodo {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    match subcommand {
        "sync" | "install" | "uninstall" | "status" | "restore" => {}
        other => {
            eprintln!("Unknown subcommand: {other}. Use sync|install|uninstall|status|restore.");
            std::process::exit(1);
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    let sync_args = parse_sync_args(&args[2..]);

    match subcommand {
        "sync" => {
            let app_config = load_or_default_config(sync_args.config_path.as_deref())?;
            let shutdown = Arc::new(AtomicBool::new(false));
            // First signal sets the flag; second signal restores the default
            // handler so the process can be force-killed immediately.
            for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
                if let Err(e) = signal_hook::flag::register(sig, Arc::clone(&shutdown)) {
                    warn!("Failed to register signal handler for {sig}: {e}");
                }
                if let Err(e) =
                    signal_hook::flag::register_conditional_default(sig, Arc::clone(&shutdown))
                {
                    warn!("Failed to register conditional default for {sig}: {e}");
                }
            }
            sync_once(&app_config, sync_args.dry_run, &shutdown)?;
        }
        "install" => {
            let app_config = load_or_default_config(sync_args.config_path.as_deref())?;
            remtodo::launchd::install(&app_config, sync_args.config_path.as_deref())?;
        }
        "uninstall" => {
            remtodo::launchd::uninstall()?;
        }
        "status" => {
            remtodo::launchd::status();
        }
        "restore" => {
            let cli = SwiftCli::new()?;
            let state_path = resolve_state_path()?;
            let state_dir = state_path.parent().ok_or_else(|| {
                SyncError::Config("Cannot determine state directory from state path".to_string())
            })?;
            remtodo::undo::execute_restore(&cli, state_dir)?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(SyncError::Interrupted(n)) => {
            eprintln!(
                "Sync interrupted — state saved for {n} completed list(s). Re-run to finish."
            );
            std::process::exit(130);
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_sync_args;

    // Item 20: verify the --dry-run flag is correctly parsed so the caller
    // can skip all file writes and CLI calls without affecting action planning.

    #[test]
    fn parse_sync_args_dry_run_flag() {
        let args: Vec<String> = vec!["--dry-run".to_string()];
        let parsed = parse_sync_args(&args);
        assert!(parsed.dry_run, "--dry-run must set dry_run=true");
        assert!(parsed.config_path.is_none());
    }

    #[test]
    fn parse_sync_args_no_flags() {
        let args: Vec<String> = vec![];
        let parsed = parse_sync_args(&args);
        assert!(!parsed.dry_run, "default dry_run must be false");
        assert!(parsed.config_path.is_none());
    }

    #[test]
    fn parse_sync_args_config_and_dry_run() {
        let args: Vec<String> = vec![
            "--config".to_string(),
            "/tmp/my.toml".to_string(),
            "--dry-run".to_string(),
        ];
        let parsed = parse_sync_args(&args);
        assert!(parsed.dry_run);
        assert_eq!(parsed.config_path.as_deref(), Some("/tmp/my.toml"));
    }
}
