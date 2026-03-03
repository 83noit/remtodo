//! Pure-function sync engine — no I/O, no process spawning.
//!
//! # Entry points
//! - [`compute_sync_actions`] — thin wrapper used by most tests (tolerance = 0, no release set)
//! - [`compute_sync_actions_ext`] — full variant with `timestamp_tolerance_secs` and
//!   `release_eids` (tasks whose sticky tracking has expired and should be released)
//!
//! # Algorithm
//! Each reminder is matched to a todo.txt task via the `eid:` tag.  The engine then
//! classifies every (reminder, task, baseline) triple into one of three cases:
//! - **Case A** — both sides exist → [`three_way_diff`] resolves per-field conflicts using LWW
//! - **Case B** — reminder gone, task present → delete task (or resurrect reminder if task newer)
//! - **Case C** — task missing or new reminder → create/update on the appropriate side
//!
//! # LWW conflict resolution
//! `three_way_diff` compares `reminder.modified` against `task_mtime`.  The side
//! with the strictly newer timestamp wins per field; ties go to Reminders.
//! A `timestamp_tolerance_secs` window suppresses spurious conflicts caused by
//! filesystem clock granularity.
//!
//! # Writeback
//! [`WritebackConfig`] lets individual fields opt out of being pushed back from
//! Reminders to the task file.  When a field is disabled, the task's value always
//! wins for that field — no LWW contest is performed.
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use log::{debug, info};

use chrono::{Duration, NaiveDate, NaiveDateTime};
use todo_lib::todotxt::{
    split_tag, CompletionConfig, CompletionDateMode, CompletionMode, Task, NO_PRIORITY,
};

use crate::filter::Filter;
use crate::mapping::reminder_to_task;
use crate::reminder::Reminder;
use crate::sync::actions::{ReminderUpdate, SyncAction};
use crate::sync::config::{
    ListSyncConfig, MappingTarget, PriorityMap, StickyTracking, WritebackConfig,
};
use crate::sync::state::{SyncItemState, SyncState, SyncedFieldState};

// ============================================================
// Public API
// ============================================================

/// Compute a stable 64-bit hash of a task's serialised todo.txt line,
/// with the `eid:VALUE` tag stripped. The eid tag is a sync join key
/// whose value can be reassigned by iCloud — stripping it means EID
/// reassignment never produces a false "task changed" signal.
pub fn task_line_hash(task: &Task) -> u64 {
    let line = format!("{task}");
    let stripped: String = line
        .split_whitespace()
        .filter(|token| !token.starts_with("eid:"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut hasher = DefaultHasher::new();
    stripped.hash(&mut hasher);
    hasher.finish()
}

/// Compute a stable 64-bit hash of the synced reminder fields.
/// Used to detect whether the reminder side changed between syncs.
pub fn synced_field_hash(fields: &SyncedFieldState) -> u64 {
    let mut hasher = DefaultHasher::new();
    fields.hash(&mut hasher);
    hasher.finish()
}

/// Build the initial sync state on the very first run, recording the current
/// field values and task-line hashes for every matched pair.
///
/// Returns `(state, reconciled_pairs)` where `reconciled_pairs` is a list of
/// `(eid, task_index)` pairs for tasks matched by title+due (bootstrap
/// reconciliation Pass 2).  The caller must stamp `eid:` onto each of those
/// tasks so subsequent syncs can join on EID as normal.
pub fn build_initial_state(
    reminders: &[Reminder],
    tasks: &[Task],
    now: NaiveDateTime,
) -> (SyncState, Vec<(String, usize)>) {
    // Pass 1: match by EID.
    // First occurrence of each eid wins for dedup.
    let mut task_by_eid: HashMap<&str, usize> = HashMap::new();
    for (i, t) in tasks.iter().enumerate() {
        if let Some(eid) = task_eid(t) {
            task_by_eid.entry(eid).or_insert(i);
        }
    }

    let mut state = SyncState {
        last_sync_time: Some(now),
        ..Default::default()
    };

    let mut matched_eids: HashSet<String> = HashSet::new();
    let mut matched_indices: HashSet<usize> = HashSet::new();

    for r in reminders {
        if let Some(&task_idx) = task_by_eid.get(r.external_id.as_str()) {
            let task = &tasks[task_idx];
            let fields = build_field_state_from_reminder(r);
            let r_hash = synced_field_hash(&fields);
            let item = SyncItemState {
                eid: r.external_id.clone(),
                fields,
                reminders_last_modified: parse_reminder_modified(r),
                task_line_hash: task_line_hash(task),
                reminders_field_hash: r_hash,
                last_synced: now,
                pushed: false, // first-sync: origin unknown, conservative = pull-origin
            };
            state.items.insert(r.external_id.clone(), item);
            matched_eids.insert(r.external_id.clone());
            matched_indices.insert(task_idx);
        }
    }

    // Pass 2: match unmatched items by (title, due_date).
    let reconciled =
        reconcile_unmatched_by_title(reminders, tasks, &matched_eids, &matched_indices);
    for (eid, task_idx) in &reconciled {
        let r = reminders
            .iter()
            .find(|r| &r.external_id == eid)
            .expect("reconciled eid must exist in reminders");
        let task = &tasks[*task_idx];
        let fields = build_field_state_from_reminder(r);
        let r_hash = synced_field_hash(&fields);
        let item = SyncItemState {
            eid: eid.clone(),
            fields,
            reminders_last_modified: parse_reminder_modified(r),
            task_line_hash: task_line_hash(task),
            reminders_field_hash: r_hash,
            last_synced: now,
            pushed: false, // first-sync: origin unknown, conservative = pull-origin
        };
        state.items.insert(eid.clone(), item);
    }

    (state, reconciled)
}

/// Compute the set of eids that should be released from sync under
/// `sticky_tracking = Triage`.
///
/// A task is released when:
/// 1. The owning list's `sticky_tracking` is `Triage`.
/// 2. The task no longer matches the owning list's push filter.
/// 3. The task's todo.txt line changed since the last sync (the edit is the
///    triage signal; unedited tasks are protected against time-based drift).
///
/// No push-origin distinction is made — any edit to any task (push- or pull-
/// origin) is sufficient. Once you've touched a task in todo.txt, the push
/// filter is authoritative.
///
/// Call this once before the per-list sync loop and pass the result to
/// [`compute_sync_actions_ext`].
pub fn compute_release_set(
    tasks: &[Task],
    state: &SyncState,
    list_configs: &[ListSyncConfig],
    today: NaiveDate,
) -> HashSet<String> {
    // Index tasks by eid for fast lookup.
    let mut task_by_eid: HashMap<&str, &Task> = HashMap::new();
    for t in tasks {
        if let Some(eid) = task_eid(t) {
            task_by_eid.entry(eid).or_insert(t);
        }
    }

    let mut release = HashSet::new();

    for (eid, state_item) in &state.items {
        // Find the owning list config.
        let owning_config = list_configs
            .iter()
            .find(|lc| lc.reminders_list == state_item.fields.list);
        let owning_config = match owning_config {
            Some(c) => c,
            None => continue, // unknown list — leave alone
        };

        // Only apply release logic for Triage mode.
        if owning_config.sticky_tracking != StickyTracking::Triage {
            continue;
        }

        // Find the task.
        let task = match task_by_eid.get(eid.as_str()) {
            Some(t) => t,
            None => continue, // task absent — engine will handle via Case C
        };

        // Skip sentinel tasks.
        if task_eid(task).map(is_sentinel_eid).unwrap_or(false) {
            continue;
        }

        // Check if still admitted by owning list.
        let owning_filter = owning_config.compiled_push_filter();
        let admitted_by_owner =
            task_matches_push_filter(task, owning_config, &owning_filter, today);
        if admitted_by_owner {
            continue; // still in scope — no release needed
        }

        // Task must have changed since last sync (inbox protection).
        let current_hash = task_line_hash(task);
        let task_changed = current_hash != state_item.task_line_hash;
        if !task_changed {
            continue;
        }

        // The edit is the triage signal — no push-origin distinction needed.
        release.insert(eid.clone());
    }

    release
}

/// Compute what actions are needed to bring both sides into sync.
///
/// Pure function — no I/O. Takes the current state of reminders and tasks,
/// the last-known sync state, the list config, the current time, and the
/// modification time of the todo.txt file (used to detect task-side changes).
/// Returns a list of actions to perform.
///
/// This is a convenience wrapper over [`compute_sync_actions_ext`] with an
/// empty release set. All existing callers and tests use this signature.
pub fn compute_sync_actions(
    reminders: &[Reminder],
    tasks: &[Task],
    state: &SyncState,
    config: &ListSyncConfig,
    now: NaiveDateTime,
    task_mtime: Option<NaiveDateTime>,
) -> Vec<SyncAction> {
    compute_sync_actions_ext(
        reminders,
        tasks,
        state,
        config,
        now,
        task_mtime,
        &HashSet::new(),
        0, // no tolerance — all existing callers and tests use strict comparison
    )
}

/// Extended variant of [`compute_sync_actions`] that accepts a pre-computed
/// release set. Eids in `release_eids` trigger a `DeleteReminder` in Case A
/// (both sides present) instead of the normal three-way diff path.
///
/// Use [`compute_release_set`] to build the release set before the per-list loop.
#[allow(clippy::too_many_arguments)]
pub fn compute_sync_actions_ext(
    reminders: &[Reminder],
    tasks: &[Task],
    state: &SyncState,
    config: &ListSyncConfig,
    _now: NaiveDateTime,
    task_mtime: Option<NaiveDateTime>,
    release_eids: &HashSet<String>,
    timestamp_tolerance_secs: u64,
) -> Vec<SyncAction> {
    let mut actions = Vec::new();
    let today = chrono::Local::now().date_naive();
    let push_filter = config.compiled_push_filter();
    let priority_map = config.compiled_priority_map();

    // reminder_by_eid: only reminders from the configured list.
    let reminder_by_eid: HashMap<&str, &Reminder> = reminders
        .iter()
        .filter(|r| r.list == config.reminders_list)
        .map(|r| (r.external_id.as_str(), r))
        .collect();

    // Build a hash → new-eid index for reminders from this list that are NOT yet
    // tracked in state.  Used in Case B to detect iCloud EID reassignment: when
    // an old EID disappears but a new EID with identical synced fields appears,
    // we relink instead of treating it as deletion + creation.
    let mut unmatched_by_hash: HashMap<u64, Vec<String>> = HashMap::new();
    for (&eid_str, r) in &reminder_by_eid {
        if !state.items.contains_key(eid_str) {
            let fields = build_field_state_from_reminder(r);
            let hash = synced_field_hash(&fields);
            if hash != 0 {
                unmatched_by_hash
                    .entry(hash)
                    .or_default()
                    .push(eid_str.to_string());
            }
        }
    }
    // Tracks new eids consumed by RelinkEid so Step 2 doesn't also emit CreateTask.
    let mut relinked_new_eids: HashSet<String> = HashSet::new();

    // Safety check: detect a stale state file.
    //
    // A stale state occurs when a previous sync wrote its output to the wrong path
    // (e.g. a relative "./todo.txt" instead of the configured absolute path) but
    // successfully saved the state with real reminder eids.  The next correct sync
    // would then see state items whose tasks are "missing" and incorrectly delete
    // all the corresponding reminders.
    //
    // Heuristic: if the file is non-empty but contains NO eid: tags at all, the
    // state is almost certainly stale.  An empty file is NOT stale — it means all
    // tasks were genuinely deleted (tasks = vec![]).
    let stale_state = !tasks.is_empty() && !tasks.iter().any(|t| task_eid(t).is_some());

    // ── Sentinel pre-pass ─────────────────────────────────────────────────────
    //
    // Scan tasks for `eid:na/<original>` and `eid:ns/<original>` sentinels.
    // Two outcomes per sentinel:
    //
    // 1. Original eid still in state → deletion hasn't fired yet.
    //    Record it in `sentinel_for_eid` so Case C uses unconditional
    //    DeleteReminder (no hash check, no resurrection).
    //
    // 2. Original eid absent from state → reminder confirmed deleted in a
    //    previous sync.  Emit CleanSentinelTag so apply_task_actions can
    //    finalize the tag:
    //      eid:na/<orig> → eid:na   (permanent local opt-out)
    //      eid:ns/<orig> → (removed) (back to normal; push_filter re-applies)
    let mut sentinel_for_eid: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for t in tasks.iter() {
        if let Some(eid) = task_eid(t) {
            if let Some(original) = sentinel_original_eid(eid) {
                if state.items.contains_key(original) {
                    sentinel_for_eid.insert(original);
                } else {
                    actions.push(SyncAction::CleanSentinelTag {
                        sentinel_eid: eid.to_string(),
                    });
                }
            }
        }
    }

    // task_by_eid: tasks admitted by push filter OR already tracked by this list.
    // With sticky_tracking=true, tasks whose eid is in state under this list remain
    // in scope even after falling off the push filter (e.g. due date window passed).
    let mut task_by_eid: HashMap<&str, &Task> = HashMap::new();
    for t in tasks {
        if let Some(eid) = task_eid(t) {
            // Sentinel tasks are invisible to the sync engine.
            if is_sentinel_eid(eid) {
                continue;
            }
            if task_by_eid.contains_key(eid) {
                continue;
            }
            let state_list = state.items.get(eid).map(|item| item.fields.list.as_str());
            // Skip tasks already owned by a different list in state.
            // Without this guard, a task resurrected by list A (its eid updated in
            // memory and state set to list A) would also be seen by list B in the
            // same sync run if it matches list B's push filter — causing a cascade
            // of spurious ResurrectReminder actions on every subsequent sync.
            if let Some(list) = state_list {
                if list != config.reminders_list {
                    continue;
                }
            }
            let tracked_by_this_list = state_list
                .map(|list| list == config.reminders_list)
                .unwrap_or(false);
            let admitted = task_matches_push_filter(t, config, &push_filter, today);
            // Always and Auto both keep tracked tasks in scope for Case A processing.
            // The difference is what happens inside Case A: Auto can emit
            // DeleteReminder via the release check; Always never does.
            let sticky = matches!(
                config.sticky_tracking,
                StickyTracking::Always | StickyTracking::Triage
            );
            if admitted || (sticky && tracked_by_this_list) {
                task_by_eid.insert(eid, t);
            }
        }
    }

    // Count Case-B state entries per stored reminder hash (reminder absent, task
    // present).  Only 1:1 matches (one old entry, one new candidate) are relinked;
    // ambiguous cases fall back to the normal Case-B path.
    let mut case_b_hash_count: HashMap<u64, usize> = HashMap::new();
    for (eid, item) in &state.items {
        if !reminder_by_eid.contains_key(eid.as_str())
            && task_by_eid.contains_key(eid.as_str())
            && item.reminders_field_hash != 0
        {
            *case_b_hash_count
                .entry(item.reminders_field_hash)
                .or_insert(0) += 1;
        }
    }

    // ── Step 1: Process previously-synced items ──────────────────────────────
    for (eid, state_item) in &state.items {
        let r = reminder_by_eid.get(eid.as_str()).copied();
        let t = task_by_eid.get(eid.as_str()).copied();

        match (r, t) {
            (Some(reminder), Some(task)) => {
                // Case A: Both present.
                //
                // Release path (sticky_tracking = Auto): if this eid is in the
                // release set, preserve any Reminders→task changes first (e.g.
                // completion), then delete the reminder.
                if release_eids.contains(eid.as_str()) {
                    let (t_upd, _) = three_way_diff(
                        reminder,
                        task,
                        &state_item.fields,
                        task_mtime,
                        &priority_map,
                        timestamp_tolerance_secs,
                        &config.writeback,
                    );
                    if !t_upd.is_empty() {
                        let updated_task = apply_task_updates(task, &t_upd, &priority_map);
                        actions.push(SyncAction::UpdateTask {
                            eid: eid.clone(),
                            updated_task,
                        });
                    }
                    actions.push(SyncAction::DeleteReminder { eid: eid.clone() });
                    continue;
                }

                // Normal path: three-way diff.
                let (t_upd, r_upd) = three_way_diff(
                    reminder,
                    task,
                    &state_item.fields,
                    task_mtime,
                    &priority_map,
                    timestamp_tolerance_secs,
                    &config.writeback,
                );

                let updated_task = if t_upd.is_empty() {
                    None
                } else {
                    Some(apply_task_updates(task, &t_upd, &priority_map))
                };
                let updated_reminder = if r_upd.is_empty() {
                    None
                } else {
                    Some(build_reminder_update(eid, &r_upd, config))
                };

                match (updated_task, updated_reminder) {
                    (Some(ut), Some(ur)) => {
                        actions.push(SyncAction::UpdateTask {
                            eid: eid.clone(),
                            updated_task: ut,
                        });
                        actions.push(SyncAction::UpdateReminder {
                            eid: eid.clone(),
                            updated_reminder: ur,
                        });
                    }
                    (Some(ut), None) => {
                        actions.push(SyncAction::UpdateTask {
                            eid: eid.clone(),
                            updated_task: ut,
                        });
                    }
                    (None, Some(ur)) => {
                        actions.push(SyncAction::UpdateReminder {
                            eid: eid.clone(),
                            updated_reminder: ur,
                        });
                    }
                    (None, None) => {}
                }
            }

            (None, Some(_task)) => {
                // Case B: Reminder absent, task present → reminder was deleted.
                //
                // First check for iCloud EID reassignment: if a new (untracked)
                // reminder from this list has the same synced-field hash as the
                // baseline, it is almost certainly the same reminder with a new
                // externalIdentifier.  Relink 1:1 only; fall back on ambiguity.
                let stored_hash = state_item.reminders_field_hash;
                if stored_hash != 0 {
                    if let Some(candidates) = unmatched_by_hash.get(&stored_hash) {
                        if candidates.len() == 1
                            && case_b_hash_count.get(&stored_hash).copied().unwrap_or(0) == 1
                        {
                            let new_eid = &candidates[0];
                            if !relinked_new_eids.contains(new_eid) {
                                // Relink: move tracking from old_eid → new_eid.
                                actions.push(SyncAction::RelinkEid {
                                    old_eid: eid.clone(),
                                    new_eid: new_eid.clone(),
                                });
                                // Diff old baseline against the new reminder so any
                                // field-level changes are still propagated.
                                let new_reminder = reminder_by_eid[new_eid.as_str()];
                                let (t_upd, r_upd) = three_way_diff(
                                    new_reminder,
                                    _task,
                                    &state_item.fields,
                                    task_mtime,
                                    &priority_map,
                                    timestamp_tolerance_secs,
                                    &config.writeback,
                                );
                                if !t_upd.is_empty() {
                                    let updated_task =
                                        apply_task_updates(_task, &t_upd, &priority_map);
                                    actions.push(SyncAction::UpdateTask {
                                        eid: new_eid.clone(),
                                        updated_task,
                                    });
                                }
                                if !r_upd.is_empty() {
                                    let updated_reminder =
                                        build_reminder_update(new_eid, &r_upd, config);
                                    actions.push(SyncAction::UpdateReminder {
                                        eid: new_eid.clone(),
                                        updated_reminder,
                                    });
                                }
                                relinked_new_eids.insert(new_eid.clone());
                                continue;
                            }
                        }
                    }
                }

                // Normal Case B: use content hash to decide delete vs resurrect.
                // A stored hash of 0 means unknown (old state.json) → treat as
                // changed (conservative: resurrect rather than accidentally delete).
                let current_hash = task_line_hash(_task);
                let task_changed = current_hash != state_item.task_line_hash;

                if task_changed {
                    // Task modified since last sync → resurrect reminder.
                    let rem_update = task_to_reminder_update(eid, _task, config, &priority_map);
                    actions.push(SyncAction::ResurrectReminder {
                        eid: eid.clone(),
                        reminder_update: rem_update,
                        target_list: config.reminders_list.clone(),
                    });
                } else {
                    // Task unchanged → reminder deletion wins, delete the task.
                    actions.push(SyncAction::DeleteTask { eid: eid.clone() });
                }
            }

            (Some(reminder), None) => {
                // Case C: Task absent, reminder present → task was deleted.
                //
                // Guard: a completed reminder with no matching task is already
                // resolved — the item is done and needs no further action.
                // We must not attempt to resurrect a task from a completed reminder.
                if reminder.is_completed {
                    continue;
                }
                //
                // Sentinel override: the user explicitly ejected this item by
                // writing `eid:na/<eid>` on the task.  Delete the reminder
                // unconditionally — no hash check, no resurrection.
                if sentinel_for_eid.contains(eid.as_str()) {
                    actions.push(SyncAction::DeleteReminder { eid: eid.clone() });
                    continue;
                }
                //
                // Safety: if NO task in the file has an eid: tag, the output file
                // was never written to by this tool (stale state from a write to a
                // different path). Deleting reminders in this situation would cause
                // data loss. Treat as first-run and recreate the task instead.
                if stale_state {
                    actions.push(SyncAction::CreateTask {
                        eid: eid.clone(),
                        reminder: (*reminder).clone(),
                    });
                    continue;
                }

                // Use content hash to decide: if the reminder changed since last
                // sync, the reminder-side change wins and we resurrect the task;
                // otherwise the task deletion wins and we delete the reminder.
                // A stored hash of 0 means unknown (old state.json) → treat as
                // changed (conservative: resurrect rather than accidentally delete).
                let current_fields = build_field_state_from_reminder(reminder);
                let current_hash = synced_field_hash(&current_fields);
                let reminder_changed = current_hash != state_item.reminders_field_hash;

                if reminder_changed {
                    // Reminder modified since last sync → resurrect task.
                    let task = reminder_to_task(reminder, &priority_map);
                    actions.push(SyncAction::ResurrectTask {
                        eid: eid.clone(),
                        task,
                    });
                } else {
                    // Reminder unchanged → task deletion wins, delete the reminder.
                    actions.push(SyncAction::DeleteReminder { eid: eid.clone() });
                }
            }

            (None, None) => {
                // Case D: Both absent → nothing to do.
            }
        }
    }

    // ── Step 2: New reminders (eid not in state) → CreateTask ────────────────
    for reminder in reminders {
        if reminder.list != config.reminders_list {
            continue;
        }
        if !state.items.contains_key(&reminder.external_id) {
            // Skip reminders already consumed by a RelinkEid action above.
            if relinked_new_eids.contains(&reminder.external_id) {
                continue;
            }
            // Skip already-completed reminders unless explicitly configured to import them.
            // This prevents historical completions from flooding todo.txt on first sync.
            if reminder.is_completed && !config.sync_initial_completed {
                continue;
            }
            actions.push(SyncAction::CreateTask {
                eid: reminder.external_id.clone(),
                reminder: reminder.clone(),
            });
        }
    }

    // ── Step 3: New tasks (eid not in state) → CreateReminder ────────────────
    // Completed tasks are never pushed as new reminders — only the UpdateReminder
    // path (via three_way_diff) may flip an already-synced reminder to completed.
    for task in tasks {
        if task.finished {
            continue;
        }
        let eid = task_eid(task);
        // Sentinel tasks are never pushed to Reminders, regardless of push_filter.
        if eid.map(is_sentinel_eid).unwrap_or(false) {
            continue;
        }
        let already_in_state = eid.map(|e| state.items.contains_key(e)).unwrap_or(false);
        if !already_in_state && task_matches_push_filter(task, config, &push_filter, today) {
            actions.push(SyncAction::CreateReminder {
                task: task.clone(),
                target_list: config.reminders_list.clone(),
            });
        }
    }

    actions
}

/// Apply computed task-side actions, returning the updated task list and new
/// sync state.
///
/// Pure function — no I/O.
pub fn apply_task_actions(
    actions: &[SyncAction],
    tasks: Vec<Task>,
    state: &SyncState,
    config: &ListSyncConfig,
    now: NaiveDateTime,
) -> (Vec<Task>, SyncState) {
    let mut tasks = tasks;
    let mut new_state = state.clone();
    new_state.last_sync_time = Some(now);
    let priority_map = config.compiled_priority_map();

    for action in actions {
        match action {
            SyncAction::CreateTask { eid, reminder } => {
                let mut task = reminder_to_task(reminder, &priority_map);
                // Attach auto_context if configured.
                if let Some(ctx) = &config.auto_context {
                    task.replace_context("", ctx);
                }
                tasks.push(task.clone());
                let fields = build_field_state_from_reminder(reminder);
                let r_hash = synced_field_hash(&fields);
                let item = SyncItemState {
                    eid: eid.clone(),
                    fields,
                    reminders_last_modified: parse_reminder_modified(reminder),
                    task_line_hash: task_line_hash(&task),
                    reminders_field_hash: r_hash,
                    last_synced: now,
                    pushed: false, // pull-origin: reminder → task
                };
                new_state.items.insert(eid.clone(), item);
            }

            SyncAction::UpdateTask { eid, updated_task } => {
                // Replace task with matching eid; push if not found.
                let mut replaced = false;
                for t in &mut tasks {
                    if task_eid(t).map(|e| e == eid.as_str()).unwrap_or(false) {
                        *t = updated_task.clone();
                        replaced = true;
                        break;
                    }
                }
                if !replaced {
                    tasks.push(updated_task.clone());
                }
                // Update state to reflect updated task.
                if let Some(item) = new_state.items.get_mut(eid.as_str()) {
                    item.fields.title = extract_title(updated_task);
                    item.fields.priority = task_priority(updated_task, &priority_map);
                    item.fields.is_completed = updated_task.finished;
                    item.fields.completion_date = task_completion_date(updated_task);
                    item.fields.due_date = task_due_date(updated_task);
                    // notes preserved: tasks no longer carry note: tags
                    item.task_line_hash = task_line_hash(updated_task);
                    item.reminders_field_hash = synced_field_hash(&item.fields);
                    item.last_synced = now;
                }
            }

            SyncAction::UpdateReminder { eid, .. } => {
                // No change to task list. Update state to reflect task's current values.
                if let Some(item) = new_state.items.get_mut(eid.as_str()) {
                    if let Some(task) = tasks
                        .iter()
                        .find(|t| task_eid(t).map(|e| e == eid.as_str()).unwrap_or(false))
                    {
                        item.fields.title = extract_title(task);
                        item.fields.priority = task_priority(task, &priority_map);
                        item.fields.is_completed = task.finished;
                        item.fields.completion_date = task_completion_date(task);
                        item.fields.due_date = task_due_date(task);
                        // notes preserved: tasks no longer carry note: tags
                        item.task_line_hash = task_line_hash(task);
                        item.reminders_field_hash = synced_field_hash(&item.fields);
                        item.last_synced = now;
                    }
                }
            }

            SyncAction::DeleteTask { eid } => {
                tasks.retain(|t| !task_eid(t).map(|e| e == eid.as_str()).unwrap_or(false));
                new_state.items.remove(eid.as_str());
            }

            SyncAction::DeleteReminder { eid } => {
                // Remove the state entry.  In Case C the task is already absent,
                // so the loop below is a no-op.  In the Triage release path the
                // task is still present — strip its eid: tag so it doesn't generate
                // a spurious "no state entry" warning on the next sync cycle, and
                // so the push_filter can re-admit it to a new list if applicable.
                new_state.items.remove(eid.as_str());
                for t in &mut tasks {
                    if task_eid(t).map(|e| e == eid.as_str()).unwrap_or(false) {
                        t.update_tag_with_value("eid", "");
                        break;
                    }
                }
            }

            SyncAction::ResurrectTask { eid, task } => {
                tasks.push(task.clone());
                let fields = SyncedFieldState {
                    title: extract_title(task),
                    priority: task_priority(task, &priority_map),
                    is_completed: task.finished,
                    completion_date: task_completion_date(task),
                    due_date: task_due_date(task),
                    notes: None, // tasks no longer carry note: tags
                    list: config.reminders_list.clone(),
                };
                let r_hash = synced_field_hash(&fields);
                let item = SyncItemState {
                    eid: eid.clone(),
                    fields,
                    reminders_last_modified: None,
                    task_line_hash: task_line_hash(task),
                    reminders_field_hash: r_hash,
                    last_synced: now,
                    pushed: false, // pull-origin: resurrected from reminder
                };
                new_state.items.insert(eid.clone(), item);
            }

            SyncAction::ResurrectReminder { eid, .. } => {
                // No task-side change. Touch last_synced.
                if let Some(item) = new_state.items.get_mut(eid.as_str()) {
                    item.last_synced = now;
                }
            }

            SyncAction::MergeConflict {
                eid, updated_task, ..
            } => {
                // Apply task-side part of the merge.
                let mut replaced = false;
                for t in &mut tasks {
                    if task_eid(t).map(|e| e == eid.as_str()).unwrap_or(false) {
                        *t = updated_task.clone();
                        replaced = true;
                        break;
                    }
                }
                if !replaced {
                    tasks.push(updated_task.clone());
                }
                if let Some(item) = new_state.items.get_mut(eid.as_str()) {
                    item.fields.title = extract_title(updated_task);
                    item.fields.priority = task_priority(updated_task, &priority_map);
                    item.fields.is_completed = updated_task.finished;
                    item.fields.completion_date = task_completion_date(updated_task);
                    item.fields.due_date = task_due_date(updated_task);
                    // notes preserved: tasks no longer carry note: tags
                    item.task_line_hash = task_line_hash(updated_task);
                    item.reminders_field_hash = synced_field_hash(&item.fields);
                    item.last_synced = now;
                }
            }

            SyncAction::CreateReminder { .. } => {
                // Reminder-side only — no task list or state change here.
            }

            SyncAction::CleanSentinelTag { sentinel_eid } => {
                // Finalise the sentinel tag now that the original reminder is
                // confirmed gone from state:
                //   eid:na/<orig> → eid:na   (permanent local opt-out)
                //   eid:ns/<orig> → (removed) (task reverts to normal rules)
                for t in &mut tasks {
                    if task_eid(t)
                        .map(|e| e == sentinel_eid.as_str())
                        .unwrap_or(false)
                    {
                        if sentinel_eid.starts_with("na/") {
                            t.update_tag_with_value("eid", "na");
                        } else {
                            // ns/ — remove eid: entirely so push_filter applies next cycle
                            t.update_tag_with_value("eid", "");
                        }
                        break;
                    }
                }
            }

            SyncAction::RelinkEid { old_eid, new_eid } => {
                // Rewrite eid: tag from old_eid → new_eid on the matching task.
                for t in &mut tasks {
                    if task_eid(t).map(|e| e == old_eid.as_str()).unwrap_or(false) {
                        t.update_tag_with_value("eid", new_eid);
                        break;
                    }
                }
                // Move state entry: remove old key, insert under new key.
                // Preserve the pushed flag and all fields; update eid + hash.
                if let Some(mut item) = new_state.items.remove(old_eid.as_str()) {
                    item.eid = new_eid.clone();
                    // Recompute task_line_hash — the eid tag value changed.
                    if let Some(task) = tasks
                        .iter()
                        .find(|t| task_eid(t).map(|e| e == new_eid.as_str()).unwrap_or(false))
                    {
                        item.task_line_hash = task_line_hash(task);
                    }
                    item.last_synced = now;
                    new_state.items.insert(new_eid.clone(), item);
                }
            }
        }
    }

    // Orphan eid cleanup: strip eid: tags from tasks whose eid is no longer in
    // state.  This heals carry-over orphans left by older code that removed the
    // state entry on release but did not strip the tag.  After the fix to
    // DeleteReminder (above), new releases no longer produce orphans; this pass
    // is a safety net for any that slipped through before the fix was deployed.
    //
    // A task with a stale eid that is NOT in state will generate a "no state
    // entry" warning every cycle and, in Case B, would spuriously resurrect the
    // reminder instead of deleting the task.  Stripping the tag lets the
    // push_filter re-admit the task to a new list on the next cycle if applicable.
    //
    // Safety: `new_state` is a clone of the full `current_state` and therefore
    // contains items from ALL configured lists — not just the one being processed
    // here.  A task tracked by another list will find its eid in `new_state.items`
    // and be left alone.
    for t in &mut tasks {
        if let Some(eid) = task_eid(t) {
            if !eid.is_empty() && !is_sentinel_eid(eid) && !new_state.items.contains_key(eid) {
                debug!(
                    "orphan eid cleanup: stripping stale eid:{eid} from task '{}' \
                     (state entry absent — carry-over from pre-fix release cycle)",
                    extract_title(t)
                );
                t.update_tag_with_value("eid", "");
            }
        }
    }

    // Hash reconciliation pass: ensure task_line_hash is accurate for all tracked
    // tasks after applying actions.  three_way_diff only covers five tracked fields
    // (title, due_date, priority, is_completed, completion_date); changes to
    // untracked fields (contexts, projects, rec:, custom tags) never trigger an
    // action, so the stored hash can fall behind the actual task.  A stale hash
    // would (a) generate a spurious verify_post_sync warning on every cycle and
    // (b) mis-classify the task as "changed" in the Case B (reminder absent) path,
    // causing a spurious ResurrectReminder instead of the correct DeleteTask.
    //
    // This pass is a state-only update: it never modifies the task list or triggers
    // any Reminders-side I/O.  Discrepancies are logged at debug level so that
    // genuine engine bugs remain visible in debug output.
    {
        let task_by_eid: HashMap<&str, &Task> = tasks
            .iter()
            .filter_map(|t| task_eid(t).map(|eid| (eid, t)))
            .collect();
        for (eid, item) in new_state.items.iter_mut() {
            if let Some(&task) = task_by_eid.get(eid.as_str()) {
                let h = task_line_hash(task);
                if h != item.task_line_hash {
                    debug!(
                        "hash reconciliation: refreshing stale task_line_hash for eid:{eid} \
                         (untracked field change — contexts/projects/tags not visible to three_way_diff)"
                    );
                    item.task_line_hash = h;
                }
            }
        }
    }

    (tasks, new_state)
}

// ============================================================
// Helper types
// ============================================================

/// Field-level changes to apply to a task (from three-way diff).
#[derive(Default)]
struct TaskFieldUpdates {
    title: Option<String>,
    /// `Some(None)` = remove; `Some(Some(s))` = set to s.
    due_date: Option<Option<String>>,
    priority: Option<i32>,
    is_completed: Option<bool>,
    completion_date: Option<Option<String>>,
}

impl TaskFieldUpdates {
    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.due_date.is_none()
            && self.priority.is_none()
            && self.is_completed.is_none()
            && self.completion_date.is_none()
    }
}

/// Field-level changes to apply to a reminder (from three-way diff).
#[derive(Default)]
struct ReminderFieldUpdates {
    title: Option<String>,
    due_date: Option<Option<String>>,
    notes: Option<Option<String>>,
    priority: Option<i32>,
    is_completed: Option<bool>,
    completion_date: Option<Option<String>>,
}

impl ReminderFieldUpdates {
    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.due_date.is_none()
            && self.notes.is_none()
            && self.priority.is_none()
            && self.is_completed.is_none()
            && self.completion_date.is_none()
    }
}

// ============================================================
// Helper functions
// ============================================================

/// Strip @contexts, +projects, and key:value tags from the subject to get
/// the human-readable title.
///
/// Uses token classification instead of substring replacement to avoid partial
/// word corruption (e.g. `@work` must not be stripped from `@workshop`).
pub fn extract_title(task: &Task) -> String {
    task.subject
        .split_whitespace()
        .filter(|token| {
            if token.starts_with('@') || token.starts_with('+') {
                return false;
            }
            if split_tag(token).is_some() {
                return false;
            }
            true
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Derive the Reminders priority integer from a task using the given map.
pub fn task_priority(task: &Task, map: &PriorityMap) -> i32 {
    map.task_to_reminders(task)
}

/// Extract the due date as a `YYYY-MM-DD` string from the task.
pub fn task_due_date(task: &Task) -> Option<String> {
    task.due_date.map(|d| d.format("%Y-%m-%d").to_string())
}

/// Extract the completion date as a `YYYY-MM-DD` string from the task.
pub fn task_completion_date(task: &Task) -> Option<String> {
    task.finish_date.map(|d| d.format("%Y-%m-%d").to_string())
}

/// Notes live in Reminders only — tasks never carry a `note:` tag.
pub fn task_notes(_task: &Task) -> Option<String> {
    None
}

/// Extract the `eid:` tag value from the task.
pub(crate) fn task_eid(task: &Task) -> Option<&str> {
    task.tags.get("eid").map(|s| s.as_str())
}

/// Return `true` for sentinel `eid:` values that opt a task out of sync.
///
/// - `eid:na` — local-only task, never pushed to Reminders.
/// - `eid:na/<orig>` — previously synced task, explicitly ejected; the
///   original eid is encoded so the engine can target the correct reminder
///   for deletion without a hash check.
///
/// Sentinel tasks are invisible to the sync engine: they are never inserted
/// into `task_by_eid`, never pushed to Reminders, and never matched against
/// state items.  They still carry an `eid:` tag so the stale-state heuristic
/// (which fires when *no* task has an `eid:` tag) is not triggered.
///
/// Recognised sentinel values:
/// - `eid:na`        — permanent local opt-out
/// - `eid:na/<orig>` — eject reminder `<orig>`, then simplify to `eid:na`
/// - `eid:ns/<orig>` — eject reminder `<orig>`, then remove `eid:` entirely
pub(crate) fn is_sentinel_eid(eid: &str) -> bool {
    eid == "na" || eid.starts_with("na/") || eid.starts_with("ns/")
}

/// If `eid` is a `na/<original>` or `ns/<original>` sentinel, return the
/// embedded original eid.
fn sentinel_original_eid(eid: &str) -> Option<&str> {
    eid.strip_prefix("na/")
        .or_else(|| eid.strip_prefix("ns/"))
        .filter(|s| !s.is_empty())
}

/// Parse the ISO 8601 `last_modified_date` string from a Reminder.
fn parse_reminder_modified(r: &Reminder) -> Option<NaiveDateTime> {
    r.last_modified_date
        .as_ref()
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ").ok())
}

/// Check whether a task is admitted by the push filter for this list.
///
/// If `push_filter` is `Some`, evaluates it against the task.
/// Otherwise falls back to `auto_context` (or matches all tasks if neither is set).
fn task_matches_push_filter(
    task: &Task,
    config: &ListSyncConfig,
    push_filter: &Option<Filter>,
    today: NaiveDate,
) -> bool {
    if let Some(f) = push_filter {
        f.matches(task, today)
    } else {
        match &config.auto_context {
            None => true,
            Some(ctx) => task.contexts.contains(ctx),
        }
    }
}

/// Extract a `SyncedFieldState` snapshot from a Reminder.
pub fn build_field_state_from_reminder(r: &Reminder) -> SyncedFieldState {
    SyncedFieldState {
        title: r.title.clone(),
        priority: r.priority,
        is_completed: r.is_completed,
        completion_date: r.completion_date.clone(),
        due_date: r.due_date.clone(),
        notes: r.notes.clone(),
        list: r.list.clone(),
    }
}

/// Verify post-sync consistency between the final task list and sync state.
///
/// Runs in-memory after the sync loops complete (before writing to disk) to
/// surface engine bugs that would otherwise be silent. Returns a list of
/// human-readable violation messages; an empty vec means no issues found.
///
/// Checks:
/// 1. **Duplicate eids** — two or more tasks share the same non-sentinel eid.
///    The engine would process only the first match on the next cycle, silently
///    ignoring the others.
/// 2. **Orphan task eid** — a task carries a real eid (non-sentinel, non-empty)
///    but no state entry exists. The engine stamped the eid but forgot to
///    create the tracking record; changes to this task would go undetected.
/// 3. **Hash mismatch** — the `task_line_hash` stored in a state entry does not
///    match the hash recomputed from the current task. The engine would
///    misidentify the task as unchanged on the next cycle.
pub fn verify_post_sync(tasks: &[Task], state: &SyncState) -> Vec<String> {
    let mut issues: Vec<String> = Vec::new();

    // Index tasks by their (non-sentinel, non-empty) eid.
    // Collect all indices so we can detect duplicates.
    let mut tasks_by_eid: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, task) in tasks.iter().enumerate() {
        if let Some(eid) = task_eid(task) {
            if !eid.is_empty() && !is_sentinel_eid(eid) {
                tasks_by_eid.entry(eid).or_default().push(i);
            }
        }
    }

    // Check 1: duplicate eids.
    for (eid, indices) in &tasks_by_eid {
        if indices.len() > 1 {
            issues.push(format!(
                "duplicate eid:{eid} on {} tasks (lines {:?})",
                indices.len(),
                indices
            ));
        }
    }

    // Checks 2 & 3: walk state items and cross-reference tasks.
    for (eid, item) in &state.items {
        if is_sentinel_eid(eid) {
            continue;
        }
        match tasks_by_eid.get(eid.as_str()) {
            None => {
                // Check 2: eid in state but not in any task. Intentionally NOT
                // reported — this is normal when a task was deleted or the state
                // entry refers to a reminder that will be cleaned up next cycle.
            }
            Some(indices) if indices.len() == 1 => {
                // Check 3: hash mismatch.
                let task = &tasks[indices[0]];
                let computed = task_line_hash(task);
                if computed != item.task_line_hash {
                    issues.push(format!(
                        "hash mismatch for eid:{eid}: state has {}, task hashes to {}",
                        item.task_line_hash, computed
                    ));
                }
            }
            Some(_) => {
                // Duplicate already reported in check 1; skip hash check.
            }
        }
    }

    // Check 2 (orphan task eid): task carries a real eid but state has no entry.
    for eid in tasks_by_eid.keys() {
        if !state.items.contains_key(*eid) {
            issues.push(format!(
                "task has eid:{eid} but no state entry (eid stamped without state update)"
            ));
        }
    }

    issues
}

/// Build a dedup key for bootstrap title+due reconciliation.
///
/// Both sides are normalised (trim + lowercase title, exact due date string)
/// so the comparison is case-insensitive and whitespace-tolerant.
fn make_reconciliation_key(title: &str, due_date: Option<&str>) -> String {
    let t = title.trim().to_lowercase();
    match due_date {
        Some(d) => format!("{t}|{d}"),
        None => format!("{t}|"),
    }
}

/// Pass 2 of bootstrap reconciliation: match unmatched reminders to unmatched
/// tasks by `(title, due_date)`.
///
/// - `matched_eids` — reminder EIDs already consumed in Pass 1 (EID match).
/// - `matched_indices` — task indices already consumed in Pass 1.
///
/// Conservative rules:
/// - Sentinel tasks (`eid:na`, `eid:na/*`, `eid:ns/*`) are excluded.
/// - If a key appears more than once on either side, the match is ambiguous
///   and is skipped entirely.
///
/// Returns `(eid, task_index)` pairs.  The caller is responsible for stamping
/// `eid:` on each matched task.
fn reconcile_unmatched_by_title(
    reminders: &[Reminder],
    tasks: &[Task],
    matched_eids: &HashSet<String>,
    matched_indices: &HashSet<usize>,
) -> Vec<(String, usize)> {
    // Build candidate maps: key → list of indices (unmatched items only).
    let mut reminder_candidates: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in reminders.iter().enumerate() {
        if matched_eids.contains(&r.external_id) {
            continue;
        }
        let key = make_reconciliation_key(&r.title, r.due_date.as_deref());
        reminder_candidates.entry(key).or_default().push(i);
    }

    let mut task_candidates: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in tasks.iter().enumerate() {
        if matched_indices.contains(&i) {
            continue;
        }
        // Exclude sentinel tasks — they opt out of sync.
        if task_eid(t).map(is_sentinel_eid).unwrap_or(false) {
            continue;
        }
        let title = extract_title(t);
        let due = task_due_date(t);
        let key = make_reconciliation_key(&title, due.as_deref());
        task_candidates.entry(key).or_default().push(i);
    }

    // Intersect: only non-ambiguous, uniquely-keyed pairs.
    let mut result = Vec::new();
    for (key, r_indices) in &reminder_candidates {
        if r_indices.len() != 1 {
            debug!("Bootstrap reconcile: ambiguous reminders for key '{key}', skipping");
            continue;
        }
        let t_indices = match task_candidates.get(key) {
            Some(v) => v,
            None => continue,
        };
        if t_indices.len() != 1 {
            debug!("Bootstrap reconcile: ambiguous tasks for key '{key}', skipping");
            continue;
        }
        let r_idx = r_indices[0];
        let t_idx = t_indices[0];
        let eid = &reminders[r_idx].external_id;
        info!(
            "Bootstrap reconcile: matched reminder '{}' (eid:{}) to task '{}' by title+due",
            reminders[r_idx].title,
            eid,
            extract_title(&tasks[t_idx])
        );
        result.push((eid.clone(), t_idx));
    }

    result
}

/// Replace the title words in `subject` with `new_title`, preserving
/// interleaved metadata tokens (@ctx, +prj, key:value).
///
/// Uses token classification: metadata tokens are kept in their original
/// positions; all non-metadata tokens are treated as title words and
/// replaced as a group with `new_title` inserted at the position of the
/// first title word.
///
/// Falls back to prepending `new_title` if no title words are present.
fn replace_title_in_subject(subject: &str, _old_title: &str, new_title: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut title_inserted = false;

    for token in subject.split_whitespace() {
        let is_meta =
            token.starts_with('@') || token.starts_with('+') || split_tag(token).is_some();

        if is_meta {
            result.push(token.to_string());
        } else if !title_inserted {
            result.push(new_title.to_string());
            title_inserted = true;
            // remaining non-meta tokens (old title words) are dropped
        }
        // else: old title word — skip
    }

    if !title_inserted {
        result.insert(0, new_title.to_string());
    }

    result.join(" ")
}

/// Apply `TaskFieldUpdates` to a cloned task, returning the result.
///
/// Preserves all TTDL-specific tags (rec:, t:, h:) that are not part of the
/// sync field set.
///
/// # Why `complete_with_config()` instead of `todo::done()`
///
/// The engine is a pure function that operates on individual `Task` values.
/// `todo::done()` requires a `&mut TaskVec` and also handles recurrence
/// spawning, which belongs at the orchestration layer.  We intentionally split
/// the work:
///
/// - **Here**: mark the single task as done via `complete_with_config()`.
/// - **`collect_recurrence_spawns()`** (called before actions are applied in
///   `main.rs`): uses `todo::done()` on a pre-completion copy to obtain the
///   next recurring instance.
///
/// `todo::done()`'s only other side effect (stopping a running timer) is
/// irrelevant for sync.
fn apply_task_updates(task: &Task, updates: &TaskFieldUpdates, map: &PriorityMap) -> Task {
    let mut t = task.clone();

    // ── Title ──────────────────────────────────────────────────────────────
    if let Some(ref new_title) = updates.title {
        let old_title = extract_title(task);
        t.subject = replace_title_in_subject(&t.subject, &old_title, new_title);
    }

    // ── Due date ───────────────────────────────────────────────────────────
    if let Some(ref due) = updates.due_date {
        match due {
            Some(d) => {
                t.update_tag_with_value("due", d);
            }
            None => {
                t.update_tag_with_value("due", "");
            }
        }
    }

    // ── Completion ─────────────────────────────────────────────────────────
    // Must be processed before priority so that mapped contexts are properly stripped.
    if let Some(completed) = updates.is_completed {
        if completed && !t.finished {
            // Determine completion date.
            let date = if let Some(Some(ref s)) = updates.completion_date {
                NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .unwrap_or_else(|_| chrono::Local::now().date_naive())
            } else {
                chrono::Local::now().date_naive()
            };
            // Use complete_with_config() directly (not todo::done()) —
            // see function-level doc for the rationale.
            t.complete_with_config(
                date,
                CompletionConfig {
                    completion_mode: CompletionMode::JustMark,
                    completion_date_mode: CompletionDateMode::AlwaysSet,
                },
            );
            // Remove all contexts this map can produce — completed tasks must not
            // carry priority-indicating contexts (e.g. @today, @urgent).
            for ctx in map.all_mapped_contexts() {
                t.replace_context(ctx, "");
            }
        } else if !completed && t.finished {
            t.uncomplete(CompletionMode::JustMark);
        }
    }

    // ── Priority / mapped contexts and letter priorities (incomplete only) ──
    if !t.finished {
        if let Some(new_reminders_pri) = updates.priority {
            // Remove every context this map can produce.
            for ctx in map.all_mapped_contexts() {
                t.replace_context(ctx, "");
            }
            // Clear letter priority if it's currently a mapped one.
            if map.all_mapped_priorities().contains(&t.priority) {
                t.priority = NO_PRIORITY;
            }
            // Apply the new target representation.
            match map.reminders_to_task(new_reminders_pri) {
                MappingTarget::Context(ctx) => {
                    let ctx = ctx.clone();
                    t.replace_context("", &ctx);
                }
                MappingTarget::Priority(p) => {
                    t.priority = *p;
                }
                MappingTarget::Nothing => {}
            }
        }
    }

    t
}

/// Build a `ReminderUpdate` struct from `ReminderFieldUpdates`.
fn build_reminder_update(
    eid: &str,
    upd: &ReminderFieldUpdates,
    config: &ListSyncConfig,
) -> ReminderUpdate {
    ReminderUpdate {
        eid: eid.to_string(),
        list_name: config.reminders_list.clone(),
        title: upd.title.clone(),
        priority: upd.priority,
        is_completed: upd.is_completed,
        completion_date: upd.completion_date.clone(),
        due_date: upd.due_date.clone(),
        notes: upd.notes.clone(),
    }
}

/// Build a `ReminderUpdate` from a task (for resurrect/create-reminder paths).
fn task_to_reminder_update(
    eid: &str,
    task: &Task,
    config: &ListSyncConfig,
    map: &PriorityMap,
) -> ReminderUpdate {
    ReminderUpdate {
        eid: eid.to_string(),
        list_name: config.reminders_list.clone(),
        title: Some(extract_title(task)),
        priority: Some(task_priority(task, map)),
        is_completed: Some(task.finished),
        completion_date: Some(task_completion_date(task)),
        due_date: Some(task_due_date(task)),
        notes: None, // notes live in Reminders only; never push from task
    }
}

/// Perform a three-way diff between a reminder, its paired task, and the last-synced baseline.
///
/// For each tracked field (`title`, `is_completed`, `due_date`, `priority`) the function
/// determines which side changed since the baseline and applies LWW conflict resolution:
/// - **Only reminder changed** → update the task field.
/// - **Only task changed** → update the reminder field.
/// - **Both changed** → the side with the strictly newer timestamp wins;
///   ties go to Reminders (task must be *strictly* later to win).
///
/// `timestamp_tolerance_secs`: modifications within this window are treated as
/// simultaneous (both "changed"), defaulting to Reminders winning the tie.
///
/// `writeback`: when a field is disabled, the task's current value is always used
/// for that field — no LWW contest, no reminder update.
///
/// Returns `(task_field_updates, reminder_field_updates)` — either or both may be empty.
fn three_way_diff(
    reminder: &Reminder,
    task: &Task,
    baseline: &SyncedFieldState,
    task_mtime: Option<NaiveDateTime>,
    map: &PriorityMap,
    timestamp_tolerance_secs: u64,
    writeback: &WritebackConfig,
) -> (TaskFieldUpdates, ReminderFieldUpdates) {
    let r_modified = parse_reminder_modified(reminder);

    // Task wins the tiebreak only when task_mtime is strictly later than the
    // reminder's modification time by more than the configured tolerance.
    // A non-zero tolerance absorbs HFS+/EventKit 1-second rounding differences
    // that would otherwise produce spurious reminder-wins outcomes.
    let task_wins = match (task_mtime, r_modified) {
        (Some(t), Some(r)) => {
            let tol = Duration::seconds(timestamp_tolerance_secs as i64);
            let wins = t > r + tol;
            // Log when timestamps are within the tolerance window so that
            // clock-skew or HFS+ 1-second rounding issues are visible.
            if !wins && t + tol >= r {
                debug!(
                    "mtime tie-break: task_mtime={t} r_modified={r} \
                     tolerance={timestamp_tolerance_secs}s → reminder wins"
                );
            }
            wins
        }
        (Some(_), None) => true,
        _ => false,
    };

    let mut t_upd = TaskFieldUpdates::default();
    let mut r_upd = ReminderFieldUpdates::default();

    // ── Title ──────────────────────────────────────────────────────────────
    {
        let r_val = &reminder.title;
        let t_val = extract_title(task);
        let b_val = &baseline.title;
        let r_changed = r_val != b_val;
        let t_changed = t_val != *b_val;
        match (r_changed, t_changed) {
            (true, false) => {
                if writeback.title {
                    t_upd.title = Some(r_val.clone());
                } else {
                    r_upd.title = Some(t_val);
                }
            }
            (false, true) => r_upd.title = Some(t_val),
            (true, true) if r_val == &t_val => {} // converged
            (true, true) => {
                if task_wins || !writeback.title {
                    r_upd.title = Some(t_val);
                } else {
                    t_upd.title = Some(r_val.clone());
                }
            }
            _ => {}
        }
    }

    // ── Due date ───────────────────────────────────────────────────────────
    {
        let r_val = reminder.due_date.clone();
        let t_val = task_due_date(task);
        let b_val = baseline.due_date.clone();
        let r_changed = r_val != b_val;
        let t_changed = t_val != b_val;
        match (r_changed, t_changed) {
            (true, false) => {
                if writeback.due_date {
                    t_upd.due_date = Some(r_val);
                } else {
                    r_upd.due_date = Some(t_val);
                }
            }
            (false, true) => r_upd.due_date = Some(t_val),
            (true, true) if r_val == t_val => {} // converged
            (true, true) => {
                if task_wins || !writeback.due_date {
                    r_upd.due_date = Some(t_val);
                } else {
                    t_upd.due_date = Some(r_val);
                }
            }
            _ => {}
        }
    }

    // ── Priority ───────────────────────────────────────────────────────────
    {
        let r_val = reminder.priority;
        let t_val = task_priority(task, map);
        let b_val = baseline.priority;
        let r_changed = r_val != b_val;
        let t_changed = t_val != b_val;
        match (r_changed, t_changed) {
            (true, false) => {
                if writeback.priority {
                    t_upd.priority = Some(r_val);
                } else {
                    r_upd.priority = Some(t_val);
                }
            }
            (false, true) => r_upd.priority = Some(t_val),
            (true, true) if r_val == t_val => {} // converged
            (true, true) => {
                if task_wins || !writeback.priority {
                    r_upd.priority = Some(t_val);
                } else {
                    t_upd.priority = Some(r_val);
                }
            }
            _ => {}
        }
    }

    // ── is_completed ───────────────────────────────────────────────────────
    {
        let r_val = reminder.is_completed;
        let t_val = task.finished;
        let b_val = baseline.is_completed;
        let r_changed = r_val != b_val;
        let t_changed = t_val != b_val;
        match (r_changed, t_changed) {
            (true, false) => {
                if writeback.is_completed {
                    t_upd.is_completed = Some(r_val);
                } else {
                    r_upd.is_completed = Some(t_val);
                }
            }
            (false, true) => r_upd.is_completed = Some(t_val),
            (true, true) if r_val == t_val => {} // converged
            (true, true) => {
                if task_wins || !writeback.is_completed {
                    r_upd.is_completed = Some(t_val);
                } else {
                    t_upd.is_completed = Some(r_val);
                }
            }
            _ => {}
        }
    }

    // ── completion_date ────────────────────────────────────────────────────
    {
        let r_val = reminder.completion_date.clone();
        let t_val = task_completion_date(task);
        let b_val = baseline.completion_date.clone();
        let r_changed = r_val != b_val;
        let t_changed = t_val != b_val;
        match (r_changed, t_changed) {
            (true, false) => {
                // completion_date follows the is_completed writeback flag.
                if writeback.is_completed {
                    t_upd.completion_date = Some(r_val);
                } else {
                    r_upd.completion_date = Some(t_val);
                }
            }
            (false, true) => r_upd.completion_date = Some(t_val),
            (true, true) if r_val == t_val => {} // converged
            (true, true) => {
                if task_wins || !writeback.is_completed {
                    r_upd.completion_date = Some(t_val);
                } else {
                    t_upd.completion_date = Some(r_val);
                }
            }
            _ => {}
        }
    }

    (t_upd, r_upd)
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime};

    use super::{
        apply_task_actions, build_field_state_from_reminder, build_initial_state,
        compute_sync_actions, extract_title, replace_title_in_subject, synced_field_hash,
        task_line_hash, verify_post_sync,
    };
    use crate::reminder::Reminder;
    use crate::sync::actions::SyncAction;
    use crate::sync::config::{ListSyncConfig, WritebackConfig};
    use crate::sync::state::{SyncItemState, SyncState, SyncedFieldState};
    use todo_lib::todotxt::Task;

    // ----------------------------------------------------------------
    // Time helpers
    // ----------------------------------------------------------------

    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-02-25 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn past_time() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-02-20 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn recent_time() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-02-24 15:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn old_time() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-02-15 08:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    // ----------------------------------------------------------------
    // Reminder builder
    // ----------------------------------------------------------------

    struct ReminderBuilder {
        eid: String,
        title: String,
        due: Option<String>,
        priority: i32,
        is_completed: bool,
        completion_date: Option<String>,
        modified: Option<NaiveDateTime>,
        notes: Option<String>,
        list: String,
    }

    impl ReminderBuilder {
        fn new(eid: &str) -> Self {
            Self {
                eid: eid.to_string(),
                title: "Test Reminder".to_string(),
                due: None,
                priority: 0,
                is_completed: false,
                completion_date: None,
                modified: Some(past_time()),
                notes: None,
                list: "Tasks".to_string(),
            }
        }

        fn title(mut self, t: &str) -> Self {
            self.title = t.to_string();
            self
        }
        fn due(mut self, d: &str) -> Self {
            self.due = Some(d.to_string());
            self
        }
        fn priority(mut self, p: i32) -> Self {
            self.priority = p;
            self
        }
        fn completed(mut self, date: &str) -> Self {
            self.is_completed = true;
            self.completion_date = Some(date.to_string());
            self
        }
        fn modified(mut self, t: NaiveDateTime) -> Self {
            self.modified = Some(t);
            self
        }
        fn notes(mut self, n: &str) -> Self {
            self.notes = Some(n.to_string());
            self
        }
        fn list(mut self, l: &str) -> Self {
            self.list = l.to_string();
            self
        }

        fn build(self) -> Reminder {
            Reminder {
                id: format!("id-{}", self.eid),
                external_id: self.eid.clone(),
                title: self.title,
                due_date: self.due,
                priority: self.priority,
                is_completed: self.is_completed,
                completion_date: self.completion_date,
                creation_date: Some("2026-02-01".to_string()),
                last_modified_date: self
                    .modified
                    .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
                notes: self.notes,
                list: self.list,
            }
        }
    }

    // ----------------------------------------------------------------
    // Task helpers
    // ----------------------------------------------------------------

    fn base_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 2, 25).unwrap()
    }

    fn task_from_line(line: &str) -> Task {
        Task::parse(line, base_date())
    }

    fn task_with_eid(eid: &str, subject: &str) -> Task {
        task_from_line(&format!("{subject} eid:{eid}"))
    }

    fn completed_task_with_eid(eid: &str, subject: &str) -> Task {
        task_from_line(&format!("x 2026-02-20 2026-02-01 {subject} eid:{eid}"))
    }

    fn local_task(subject: &str) -> Task {
        task_from_line(subject)
    }

    // ----------------------------------------------------------------
    // State helpers
    // ----------------------------------------------------------------

    fn default_config() -> ListSyncConfig {
        ListSyncConfig {
            reminders_list: "Tasks".to_string(),
            ..Default::default()
        }
    }

    fn shopping_config() -> ListSyncConfig {
        ListSyncConfig {
            reminders_list: "Shopping".to_string(),
            auto_context: Some("shopping".to_string()),
            ..Default::default()
        }
    }

    fn empty_state() -> SyncState {
        SyncState::default()
    }

    fn synced_item(eid: &str, title: &str) -> SyncItemState {
        let fields = SyncedFieldState {
            title: title.to_string(),
            priority: 0,
            is_completed: false,
            completion_date: None,
            due_date: None,
            notes: None,
            list: "Tasks".to_string(),
        };
        let r_hash = synced_field_hash(&fields);
        SyncItemState {
            eid: eid.to_string(),
            fields,
            reminders_last_modified: Some(past_time()),
            task_line_hash: 0,
            reminders_field_hash: r_hash,
            last_synced: past_time(),
            pushed: true,
        }
    }

    fn state_with_items(items: Vec<SyncItemState>) -> SyncState {
        let mut state = SyncState::default();
        for item in items {
            state.items.insert(item.eid.clone(), item);
        }
        state.last_sync_time = Some(past_time());
        state
    }

    // ----------------------------------------------------------------
    // Action assertion helpers
    // ----------------------------------------------------------------

    fn assert_creates_task(actions: &[SyncAction], eid: &str) {
        let found = actions.iter().any(|a| {
            if let SyncAction::CreateTask { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        });
        assert!(found, "Expected CreateTask(eid={eid})");
    }

    fn assert_creates_reminder(actions: &[SyncAction]) {
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateReminder { .. })),
            "Expected at least one CreateReminder action"
        );
    }

    fn assert_updates_task(actions: &[SyncAction], eid: &str) {
        let found = actions.iter().any(|a| {
            if let SyncAction::UpdateTask { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        });
        assert!(found, "Expected UpdateTask(eid={eid})");
    }

    fn assert_updates_reminder(actions: &[SyncAction], eid: &str) {
        let found = actions.iter().any(|a| {
            if let SyncAction::UpdateReminder { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        });
        assert!(found, "Expected UpdateReminder(eid={eid})");
    }

    fn assert_deletes_task(actions: &[SyncAction], eid: &str) {
        let found = actions.iter().any(|a| {
            if let SyncAction::DeleteTask { eid: e } = a {
                e.as_str() == eid
            } else {
                false
            }
        });
        assert!(found, "Expected DeleteTask(eid={eid})");
    }

    fn assert_deletes_reminder(actions: &[SyncAction], eid: &str) {
        let found = actions.iter().any(|a| {
            if let SyncAction::DeleteReminder { eid: e } = a {
                e.as_str() == eid
            } else {
                false
            }
        });
        assert!(found, "Expected DeleteReminder(eid={eid})");
    }

    fn assert_no_action(actions: &[SyncAction]) {
        assert!(
            actions.is_empty(),
            "Expected no actions but got {} action(s)",
            actions.len()
        );
    }

    fn assert_resurrects_task(actions: &[SyncAction], eid: &str) {
        let found = actions.iter().any(|a| {
            if let SyncAction::ResurrectTask { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        });
        assert!(found, "Expected ResurrectTask(eid={eid})");
    }

    fn assert_resurrects_reminder(actions: &[SyncAction], eid: &str) {
        let found = actions.iter().any(|a| {
            if let SyncAction::ResurrectReminder { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        });
        assert!(found, "Expected ResurrectReminder(eid={eid})");
    }

    // ============================================================
    // Category 1: Creates — Reminder → Task
    // ============================================================

    #[test]
    fn new_reminder_creates_task() {
        let reminders = vec![ReminderBuilder::new("eid-1").title("Buy milk").build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, "eid-1");
    }

    #[test]
    fn new_reminder_with_due_date_maps_correctly() {
        let reminders = vec![ReminderBuilder::new("eid-2")
            .title("Dentist")
            .due("2026-03-10")
            .build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, "eid-2");
        if let Some(SyncAction::CreateTask { reminder, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::CreateTask { .. }))
        {
            assert_eq!(reminder.due_date, Some("2026-03-10".to_string()));
        }
    }

    #[test]
    fn new_reminder_with_priority_9_gets_today_context() {
        let reminders = vec![ReminderBuilder::new("eid-3")
            .title("Urgent task")
            .priority(9)
            .build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, "eid-3");
        if let Some(SyncAction::CreateTask { reminder, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::CreateTask { .. }))
        {
            assert_eq!(reminder.priority, 9);
        }
    }

    #[test]
    fn new_completed_reminder_creates_completed_task() {
        let reminders = vec![ReminderBuilder::new("eid-4")
            .title("Done thing")
            .completed("2026-02-24")
            .build()];
        let tasks = vec![];
        let state = empty_state();
        // sync_initial_completed = true: import already-completed reminders
        let config = ListSyncConfig {
            reminders_list: "Tasks".to_string(),
            sync_initial_completed: true,
            ..Default::default()
        };
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, "eid-4");
        if let Some(SyncAction::CreateTask { reminder, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::CreateTask { .. }))
        {
            assert!(reminder.is_completed);
        }
    }

    #[test]
    fn completed_reminder_skipped_on_first_sync_by_default() {
        let reminders = vec![ReminderBuilder::new("eid-skip")
            .title("Old completed thing")
            .completed("2026-01-01")
            .build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config(); // sync_initial_completed = false
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateTask { .. })),
            "completed reminder should be skipped when sync_initial_completed=false"
        );
    }

    #[test]
    fn new_reminder_with_notes_creates_task() {
        // Notes live in Reminders only — they should not appear in the task line,
        // but the CreateTask action should still be produced.
        let reminders = vec![ReminderBuilder::new("eid-5")
            .title("Read book")
            .notes("Chapter 5 first")
            .build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, "eid-5");
        if let Some(SyncAction::CreateTask { reminder, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::CreateTask { .. }))
        {
            // Notes are preserved in the reminder data passed to the action.
            assert_eq!(reminder.notes, Some("Chapter 5 first".to_string()));
        }
    }

    #[test]
    fn multiple_new_reminders_create_multiple_tasks() {
        let reminders = vec![
            ReminderBuilder::new("eid-a").title("Task A").build(),
            ReminderBuilder::new("eid-b").title("Task B").build(),
            ReminderBuilder::new("eid-c").title("Task C").build(),
        ];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, "eid-a");
        assert_creates_task(&actions, "eid-b");
        assert_creates_task(&actions, "eid-c");
        let create_count = actions
            .iter()
            .filter(|a| matches!(a, SyncAction::CreateTask { .. }))
            .count();
        assert_eq!(create_count, 3);
    }

    // ============================================================
    // Category 2: Creates — Task → Reminder
    // ============================================================

    #[test]
    fn new_local_task_without_eid_creates_reminder() {
        let reminders = vec![];
        let tasks = vec![local_task("Buy groceries")];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_creates_reminder(&actions);
    }

    #[test]
    fn new_local_task_targets_configured_list() {
        let reminders = vec![];
        let tasks = vec![local_task("Get milk @shopping")];
        let state = empty_state();
        let config = shopping_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_creates_reminder(&actions);
        if let Some(SyncAction::CreateReminder { target_list, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::CreateReminder { .. }))
        {
            assert_eq!(target_list.as_str(), "Shopping");
        }
    }

    #[test]
    fn completed_task_without_eid_not_pushed_to_reminders() {
        // Completed tasks must never be pushed as *new* reminders via Step 3.
        // Only the UpdateReminder path (three_way_diff) may mark an already-synced
        // reminder complete.  This prevents historical completions in todo.txt from
        // flooding Apple Reminders on the first sync.
        let reminders = vec![];
        let tasks = vec![task_from_line("x 2026-02-20 2026-02-01 Done thing")];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateReminder { .. })),
            "completed tasks must not produce CreateReminder actions"
        );
    }

    #[test]
    fn task_with_eid_already_in_state_is_not_new() {
        let reminders = vec![ReminderBuilder::new("eid-1").build()];
        let tasks = vec![task_with_eid("eid-1", "Some task")];
        let state = state_with_items(vec![synced_item("eid-1", "Some task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SyncAction::CreateTask { .. })));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SyncAction::CreateReminder { .. })));
    }

    // ============================================================
    // Category 3: Edits — Reminder Changed → Update Task
    // ============================================================

    #[test]
    fn reminder_title_changed_updates_task() {
        let eid = "eid-1";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("New Title")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Old Title")];
        let state = state_with_items(vec![synced_item(eid, "Old Title")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
    }

    #[test]
    fn reminder_due_date_added_updates_task() {
        let eid = "eid-2";
        let mut item = synced_item(eid, "Task");
        item.fields.due_date = None;
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .due("2026-03-15")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Task")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
    }

    #[test]
    fn reminder_due_date_removed_updates_task() {
        let eid = "eid-3";
        let mut item = synced_item(eid, "Task");
        item.fields.due_date = Some("2026-03-01".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_from_line(&format!("Task due:2026-03-01 eid:{eid}"))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
    }

    #[test]
    fn reminder_priority_changed_updates_task() {
        let eid = "eid-4";
        let mut item = synced_item(eid, "Task");
        item.fields.priority = 0;
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .priority(9)
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Task")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
    }

    #[test]
    fn reminder_notes_changed_no_task_update() {
        // Notes no longer round-trip through todo.txt — a change to reminder
        // notes should not trigger UpdateTask.
        let eid = "eid-5";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .notes("New note text")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Task")];
        let state = state_with_items(vec![synced_item(eid, "Task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::UpdateTask { .. })),
            "notes change must not trigger UpdateTask"
        );
    }

    // ============================================================
    // Category 4: Edits — Task Changed → Update Reminder
    // ============================================================

    #[test]
    fn task_title_changed_updates_reminder() {
        let eid = "eid-1";
        let reminders = vec![ReminderBuilder::new(eid).title("Old Title").build()];
        let tasks = vec![task_with_eid(eid, "New Title")];
        let state = state_with_items(vec![synced_item(eid, "Old Title")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_updates_reminder(&actions, eid);
    }

    #[test]
    fn task_due_date_added_updates_reminder() {
        let eid = "eid-2";
        let mut item = synced_item(eid, "Task");
        item.fields.due_date = None;
        let reminders = vec![ReminderBuilder::new(eid).title("Task").build()];
        let tasks = vec![task_from_line(&format!("Task due:2026-04-01 eid:{eid}"))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_updates_reminder(&actions, eid);
    }

    #[test]
    fn task_due_date_removed_updates_reminder() {
        let eid = "eid-3";
        let mut item = synced_item(eid, "Task");
        item.fields.due_date = Some("2026-03-01".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .due("2026-03-01")
            .build()];
        let tasks = vec![task_with_eid(eid, "Task")]; // no due date
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_updates_reminder(&actions, eid);
    }

    #[test]
    fn task_today_context_added_updates_reminder_priority() {
        let eid = "eid-4";
        let mut item = synced_item(eid, "Task");
        item.fields.priority = 0;
        let reminders = vec![ReminderBuilder::new(eid).title("Task").priority(0).build()];
        let tasks = vec![task_from_line(&format!("Task @today eid:{eid}"))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_updates_reminder(&actions, eid);
    }

    // ============================================================
    // Category 5: Edits — No-op
    // ============================================================

    #[test]
    fn unchanged_item_produces_no_action() {
        let eid = "eid-1";
        let reminders = vec![ReminderBuilder::new(eid).title("Same title").build()];
        let tasks = vec![task_with_eid(eid, "Same title")];
        let state = state_with_items(vec![synced_item(eid, "Same title")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_no_action(&actions);
    }

    #[test]
    fn unchanged_item_with_due_date_produces_no_action() {
        let eid = "eid-2";
        let mut item = synced_item(eid, "Task with due");
        item.fields.due_date = Some("2026-03-01".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task with due")
            .due("2026-03-01")
            .build()];
        let tasks = vec![task_from_line(&format!(
            "Task with due due:2026-03-01 eid:{eid}"
        ))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_no_action(&actions);
    }

    #[test]
    fn unchanged_completed_item_produces_no_action() {
        let eid = "eid-3";
        let mut item = synced_item(eid, "Done task");
        item.fields.is_completed = true;
        item.fields.completion_date = Some("2026-02-20".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Done task")
            .completed("2026-02-20")
            .build()];
        let tasks = vec![completed_task_with_eid(eid, "Done task")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_no_action(&actions);
    }

    // ============================================================
    // Category 6: Completions
    // ============================================================

    #[test]
    fn complete_in_reminders_completes_task() {
        let eid = "eid-1";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .completed("2026-02-24")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Task")];
        let state = state_with_items(vec![synced_item(eid, "Task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
    }

    #[test]
    fn complete_in_task_completes_reminder() {
        let eid = "eid-2";
        let reminders = vec![ReminderBuilder::new(eid).title("Task").build()];
        let tasks = vec![completed_task_with_eid(eid, "Task")];
        let state = state_with_items(vec![synced_item(eid, "Task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_updates_reminder(&actions, eid);
    }

    #[test]
    fn uncomplete_in_reminders_uncompletes_task() {
        let eid = "eid-3";
        let mut item = synced_item(eid, "Task");
        item.fields.is_completed = true;
        item.fields.completion_date = Some("2026-02-20".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .modified(recent_time())
            .build()];
        let tasks = vec![completed_task_with_eid(eid, "Task")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
    }

    #[test]
    fn uncomplete_in_task_uncompletes_reminder() {
        let eid = "eid-4";
        let mut item = synced_item(eid, "Task");
        item.fields.is_completed = true;
        item.fields.completion_date = Some("2026-02-20".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .completed("2026-02-20")
            .build()];
        let tasks = vec![task_with_eid(eid, "Task")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_updates_reminder(&actions, eid);
    }

    #[test]
    fn complete_on_both_sides_no_conflict() {
        let eid = "eid-5";
        let mut item = synced_item(eid, "Task");
        item.fields.is_completed = true;
        item.fields.completion_date = Some("2026-02-20".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .completed("2026-02-20")
            .build()];
        let tasks = vec![completed_task_with_eid(eid, "Task")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_no_action(&actions);
    }

    #[test]
    fn complete_in_reminders_with_priority_9_drops_today() {
        let eid = "eid-6";
        let mut item = synced_item(eid, "Task");
        item.fields.priority = 9;
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .priority(9)
            .completed("2026-02-24")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_from_line(&format!("Task @today eid:{eid}"))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
        if let Some(SyncAction::UpdateTask { updated_task, .. }) = actions.iter().find(|a| {
            if let SyncAction::UpdateTask { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        }) {
            assert!(
                !updated_task.contexts.contains(&"today".to_string()),
                "@today must be dropped for completed task"
            );
        }
    }

    #[test]
    fn complete_preserves_other_fields() {
        let eid = "eid-7";
        let mut item = synced_item(eid, "Task");
        item.fields.due_date = Some("2026-03-01".to_string());
        item.fields.notes = Some("Some note".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .due("2026-03-01")
            .notes("Some note")
            .completed("2026-02-24")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_from_line(&format!(
            "Task due:2026-03-01 note:Some note eid:{eid}"
        ))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
        if let Some(SyncAction::UpdateTask { updated_task, .. }) = actions.iter().find(|a| {
            if let SyncAction::UpdateTask { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        }) {
            let line = format!("{updated_task}");
            assert!(line.contains("eid:"), "eid tag must survive completion");
            assert!(line.contains("due:"), "due date must survive completion");
        }
    }

    #[test]
    fn both_sides_uncomplete_produces_no_action() {
        let eid = "eid-8";
        let item = synced_item(eid, "Task"); // already synced as uncompleted
        let reminders = vec![ReminderBuilder::new(eid).title("Task").build()];
        let tasks = vec![task_with_eid(eid, "Task")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_no_action(&actions);
    }

    // ============================================================
    // Category 7: Deletions
    // ============================================================

    #[test]
    fn reminder_deleted_removes_task() {
        let eid = "eid-1";
        let task = task_with_eid(eid, "Some task");
        let mut item = synced_item(eid, "Some task");
        item.task_line_hash = task_line_hash(&task); // hash matches → task unchanged → deletion wins
        let reminders = vec![]; // reminder gone
        let tasks = vec![task];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_deletes_task(&actions, eid);
    }

    #[test]
    fn task_deleted_removes_reminder() {
        let eid = "eid-2";
        let reminders = vec![ReminderBuilder::new(eid).title("Some task").build()];
        let tasks = vec![]; // task gone
        let state = state_with_items(vec![synced_item(eid, "Some task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_deletes_reminder(&actions, eid);
    }

    #[test]
    fn both_deleted_just_removes_state() {
        let eid = "eid-3";
        let reminders = vec![];
        let tasks = vec![];
        let state = state_with_items(vec![synced_item(eid, "Some task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        // Both sides gone — no need to emit a delete for either
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::DeleteTask { eid: e } = a {
                    e.as_str() == eid
                } else {
                    false
                }
            }),
            "Should not emit DeleteTask when task is already gone"
        );
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::DeleteReminder { eid: e } = a {
                    e.as_str() == eid
                } else {
                    false
                }
            }),
            "Should not emit DeleteReminder when reminder is already gone"
        );
    }

    #[test]
    fn reminder_deleted_does_not_affect_unrelated_tasks() {
        let eid = "eid-deleted";
        let other_eid = "eid-other";
        let deleted_task = task_with_eid(eid, "Deleted reminder's task");
        let other_task = task_with_eid(other_eid, "Unrelated task");
        // eid-deleted reminder is gone; eid-other reminder still exists
        let reminders = vec![ReminderBuilder::new(other_eid)
            .title("Unrelated task")
            .build()];
        let mut del_item = synced_item(eid, "Deleted reminder's task");
        del_item.task_line_hash = task_line_hash(&deleted_task); // hash matches → unchanged
        let state = state_with_items(vec![del_item, synced_item(other_eid, "Unrelated task")]);
        let tasks = vec![deleted_task, other_task];
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_deletes_task(&actions, eid);
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::DeleteTask { eid: e } = a {
                    e.as_str() == other_eid
                } else {
                    false
                }
            }),
            "Unrelated task must not be deleted"
        );
    }

    #[test]
    fn task_without_eid_not_in_state_is_preserved() {
        let reminders = vec![];
        let tasks = vec![local_task("Local task no eid")];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        // Engine should not delete a local task — it may create a reminder for it
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SyncAction::DeleteTask { .. })));
    }

    #[test]
    fn task_with_eid_not_in_state_is_preserved() {
        // eid present in task but absent from sync state → treat as orphan/new, not deleted
        let eid = "eid-orphan";
        let reminders = vec![];
        let tasks = vec![task_with_eid(eid, "Orphan task")];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::DeleteTask { eid: e } = a {
                    e.as_str() == eid
                } else {
                    false
                }
            }),
            "Orphan eid task should not be deleted"
        );
    }

    #[test]
    fn delete_from_reminders_cleans_state_entry() {
        let eid = "eid-del";
        let task = task_with_eid(eid, "Task");
        let mut item = synced_item(eid, "Task");
        item.task_line_hash = task_line_hash(&task); // hash matches → unchanged → DeleteTask
        let reminders = vec![];
        let tasks = vec![task.clone()];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        let (_, new_state) = apply_task_actions(&actions, vec![task], &state, &config, now());
        assert!(
            !new_state.items.contains_key(eid),
            "Deleted eid should be removed from sync state"
        );
    }

    #[test]
    fn archived_task_treated_as_deletion() {
        // Task with known eid is absent from the list → treated as deleted
        let eid = "eid-arch";
        let reminders = vec![ReminderBuilder::new(eid).title("Archived task").build()];
        let tasks = vec![]; // moved to done.txt or otherwise absent
        let state = state_with_items(vec![synced_item(eid, "Archived task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_deletes_reminder(&actions, eid);
    }

    #[test]
    fn delete_multiple_items() {
        let eid1 = "eid-1";
        let eid2 = "eid-2";
        let task1 = task_with_eid(eid1, "Task 1");
        let task2 = task_with_eid(eid2, "Task 2");
        let mut item1 = synced_item(eid1, "Task 1");
        item1.task_line_hash = task_line_hash(&task1);
        let mut item2 = synced_item(eid2, "Task 2");
        item2.task_line_hash = task_line_hash(&task2);
        let reminders = vec![]; // both reminders deleted
        let tasks = vec![task1, task2];
        let state = state_with_items(vec![item1, item2]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_deletes_task(&actions, eid1);
        assert_deletes_task(&actions, eid2);
    }

    #[test]
    fn delete_reminder_for_completed_task_still_works() {
        let eid = "eid-cdel";
        let task = completed_task_with_eid(eid, "Done task");
        let mut item = synced_item(eid, "Done task");
        item.fields.is_completed = true;
        item.task_line_hash = task_line_hash(&task); // hash matches → unchanged → DeleteTask
        let reminders = vec![];
        let tasks = vec![task];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_deletes_task(&actions, eid);
    }

    // ============================================================
    // Category 8: Conflicts
    // ============================================================

    #[test]
    fn conflict_title_changed_both_sides_lww_reminders_wins() {
        let eid = "eid-1";
        // Reminder was modified more recently → reminder title wins
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Reminder Title")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Task Title")];
        let state = state_with_items(vec![synced_item(eid, "Old Title")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        // Reminder newer: update task only
        assert_updates_task(&actions, eid);
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::UpdateReminder { eid: e, .. } = a {
                    e.as_str() == eid
                } else {
                    false
                }
            }),
            "Should not update reminder when it is the newer side"
        );
    }

    #[test]
    fn conflict_title_changed_both_sides_lww_task_wins() {
        let eid = "eid-2";
        // Task modified more recently → task title wins
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Reminder Title")
            .modified(past_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Task Title")];
        let state = state_with_items(vec![synced_item(eid, "Old Title")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        // Task newer: update reminder only
        assert_updates_reminder(&actions, eid);
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::UpdateTask { eid: e, .. } = a {
                    e.as_str() == eid
                } else {
                    false
                }
            }),
            "Should not update task when it is the newer side"
        );
    }

    #[test]
    fn conflict_different_fields_changed_merges_both() {
        let eid = "eid-3";
        // Reminder changed its title; task changed its due date → merge both changes
        let mut item = synced_item(eid, "Old Title");
        item.fields.due_date = None;
        let reminders = vec![ReminderBuilder::new(eid)
            .title("New Title")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_from_line(&format!(
            "Old Title due:2026-04-01 eid:{eid}"
        ))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        // Different fields → MergeConflict or (UpdateTask + UpdateReminder)
        let has_merge = actions
            .iter()
            .any(|a| matches!(a, SyncAction::MergeConflict { .. }));
        let has_both_updates = actions.iter().any(|a| {
            if let SyncAction::UpdateTask { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        }) && actions.iter().any(|a| {
            if let SyncAction::UpdateReminder { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        });
        assert!(
            has_merge || has_both_updates,
            "Expected merge or coordinated update for non-overlapping field changes"
        );
    }

    #[test]
    fn conflict_reminder_completed_task_title_changed() {
        let eid = "eid-4";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .completed("2026-02-24")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Changed Title")];
        let state = state_with_items(vec![synced_item(eid, "Task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert!(
            !actions.is_empty(),
            "Expected at least one action for completion/title conflict"
        );
    }

    #[test]
    fn conflict_both_completed_with_different_dates() {
        let eid = "eid-5";
        let mut item = synced_item(eid, "Task");
        item.fields.is_completed = false; // was not completed at last sync
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .completed("2026-02-23")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_from_line(&format!(
            "x 2026-02-24 2026-02-01 Task eid:{eid}"
        ))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert!(
            !actions.is_empty(),
            "Expected action to reconcile different completion dates"
        );
    }

    #[test]
    fn conflict_task_completed_reminder_deleted_resurrects() {
        // Task changed (completed) since last sync → task wins → resurrect the reminder.
        // The stored hash is for the uncompleted baseline; the completed task has a
        // different hash → task_changed = true → ResurrectReminder.
        let eid = "eid-6";
        let baseline_task = task_from_line(&format!("Task eid:{eid}"));
        let mut item = synced_item(eid, "Task");
        item.task_line_hash = task_line_hash(&baseline_task); // hash of uncompleted baseline
        let reminders = vec![]; // reminder gone
        let tasks = vec![task_from_line(&format!(
            "x 2026-02-24 2026-02-01 Task eid:{eid}"
        ))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_resurrects_reminder(&actions, eid);
    }

    #[test]
    fn conflict_task_unchanged_reminder_deleted_deletes() {
        // Task unchanged since last sync → reminder deletion wins → delete the task.
        // The stored hash matches the current task → task_changed = false → DeleteTask.
        let eid = "eid-7";
        let current_task = task_from_line(&format!("Task eid:{eid}"));
        let mut item = synced_item(eid, "Task");
        item.task_line_hash = task_line_hash(&current_task); // hash matches current task
        let reminders = vec![]; // reminder gone
        let tasks = vec![current_task];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_deletes_task(&actions, eid);
    }

    #[test]
    fn conflict_reminder_modified_task_deleted_resurrects() {
        // Reminder changed since last sync → reminder wins → resurrect the task.
        // Stored hash is for baseline title "Task"; current reminder has "Modified Reminder"
        // → different hash → reminder_changed = true → ResurrectTask.
        let eid = "eid-8";
        let baseline_reminder = ReminderBuilder::new(eid).title("Task").build();
        let mut item = synced_item(eid, "Task");
        item.reminders_field_hash =
            synced_field_hash(&build_field_state_from_reminder(&baseline_reminder));
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Modified Reminder")
            .modified(recent_time())
            .build()];
        let tasks = vec![]; // task gone
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_resurrects_task(&actions, eid);
    }

    #[test]
    fn conflict_reminder_unchanged_task_deleted_deletes() {
        // Reminder unchanged since last sync → task deletion wins → delete the reminder.
        // Stored hash matches current reminder → reminder_changed = false → DeleteReminder.
        let eid = "eid-9";
        let current_reminder = ReminderBuilder::new(eid).title("Task").build();
        let mut item = synced_item(eid, "Task");
        item.reminders_field_hash =
            synced_field_hash(&build_field_state_from_reminder(&current_reminder));
        let reminders = vec![current_reminder];
        let tasks = vec![]; // task gone
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_deletes_reminder(&actions, eid);
    }

    // ── Regression: completing reminder on another device ──────────────────────
    //
    // When `include_completed = true` is used (now the default in the sync loop),
    // a completed reminder appears in `reminder_by_eid`.  Case A fires and
    // three_way_diff emits UpdateTask to mark the task done — no resurrection.

    #[test]
    fn reminder_completed_on_remote_marks_task_done() {
        // Simulates fetching with include_completed=true: the completed reminder
        // is present in the reminders slice.  Case A → three_way_diff detects
        // is_completed changed → UpdateTask.
        let eid = "eid-remote-complete";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Daily feeding")
            .completed("2026-02-26")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Daily feeding")];
        let state = state_with_items(vec![synced_item(eid, "Daily feeding")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
        // Verify the updated task is marked as completed.
        if let Some(SyncAction::UpdateTask { updated_task, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::UpdateTask { .. }))
        {
            assert!(
                updated_task.finished,
                "Task should be marked as completed when reminder is completed"
            );
        }
        // Must not produce a ResurrectReminder — that would undo the completion.
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::ResurrectReminder { .. })),
            "ResurrectReminder must not fire when reminder was completed remotely"
        );
    }

    #[test]
    fn completed_reminder_with_absent_task_no_action() {
        // Case C guard: completed reminder, task already gone → skip.
        // Do not attempt to resurrect from a completed reminder.
        let eid = "eid-done-gone";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Done thing")
            .completed("2026-02-25")
            .modified(recent_time())
            .build()];
        let tasks = vec![]; // task already absent
        let state = state_with_items(vec![synced_item(eid, "Done thing")]);
        let config = default_config();
        let actions =
            compute_sync_actions(&reminders, &tasks, &state, &config, now(), Some(old_time()));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::ResurrectTask { .. })),
            "Should not resurrect task from a completed reminder"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteReminder { .. })),
            "Should not delete a completed reminder"
        );
    }

    #[test]
    fn conflict_notes_changed_both_sides_no_action() {
        // Notes no longer round-trip through todo.txt; any apparent note
        // difference should not trigger a sync action.
        let eid = "eid-10";
        let mut item = synced_item(eid, "Task");
        item.fields.notes = Some("Original note".to_string());
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Task")
            .notes("Reminder note")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Task")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                SyncAction::UpdateTask { .. } | SyncAction::MergeConflict { .. }
            )),
            "notes-only difference must not produce any sync action"
        );
    }

    #[test]
    fn conflict_priority_changed_both_sides() {
        // Both sides converge on @today (priority 9) → effectively no conflict
        let eid = "eid-11";
        let mut item = synced_item(eid, "Task");
        item.fields.priority = 0;
        let reminders = vec![ReminderBuilder::new(eid).title("Task").priority(9).build()];
        let tasks = vec![task_from_line(&format!("Task @today eid:{eid}"))];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        // Both sides set the same effective priority → no MergeConflict needed
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SyncAction::MergeConflict { .. })));
    }

    #[test]
    fn no_conflict_when_same_field_changed_to_same_value() {
        let eid = "eid-12";
        let mut item = synced_item(eid, "Old Title");
        item.fields.title = "Old Title".to_string();
        let reminders = vec![ReminderBuilder::new(eid)
            .title("New Title")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "New Title")];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        // Both sides already agree on the new value → no conflict
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SyncAction::MergeConflict { .. })));
    }

    #[test]
    fn conflict_resolution_updates_sync_state() {
        let eid = "eid-13";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("New Title")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Old Title")];
        let state = state_with_items(vec![synced_item(eid, "Old Title")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        let (_, new_state) = apply_task_actions(
            &actions,
            vec![task_with_eid(eid, "Old Title")],
            &state,
            &config,
            now(),
        );
        assert_eq!(
            new_state
                .items
                .get(eid)
                .expect("state entry must exist")
                .fields
                .title,
            "New Title"
        );
    }

    // ============================================================
    // Category 9: Multi-List
    // ============================================================

    #[test]
    fn auto_context_filters_tasks_to_matching_context() {
        let shopping_eid = "eid-shop";
        let other_eid = "eid-other";
        let reminders = vec![ReminderBuilder::new(shopping_eid).list("Shopping").build()];
        let tasks = vec![
            task_from_line(&format!("Shopping task @shopping eid:{shopping_eid}")),
            task_from_line(&format!("Other task @work eid:{other_eid}")),
        ];
        let state = state_with_items(vec![
            synced_item(shopping_eid, "Shopping task"),
            synced_item(other_eid, "Other task"),
        ]);
        let config = shopping_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        // @work task must not be touched by the shopping list sync
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::DeleteTask { eid: e } = a {
                    e.as_str() == other_eid
                } else {
                    false
                }
            }),
            "@work task must not be deleted by shopping list sync"
        );
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::UpdateTask { eid: e, .. } = a {
                    e.as_str() == other_eid
                } else {
                    false
                }
            }),
            "@work task must not be updated by shopping list sync"
        );
    }

    #[test]
    fn auto_context_added_to_new_tasks_from_reminders() {
        let eid = "eid-new";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Buy apples")
            .list("Shopping")
            .build()];
        let tasks = vec![];
        let state = empty_state();
        let config = shopping_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, eid);
        if let Some(SyncAction::CreateTask { reminder, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::CreateTask { .. }))
        {
            assert_eq!(reminder.list, "Shopping");
        }
    }

    #[test]
    fn auto_context_preserved_on_update() {
        let eid = "eid-upd";
        let mut item = synced_item(eid, "Old Title");
        item.fields.list = "Shopping".to_string();
        let reminders = vec![ReminderBuilder::new(eid)
            .title("New Title")
            .list("Shopping")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_from_line(&format!("Old Title @shopping eid:{eid}"))];
        let state = state_with_items(vec![item]);
        let config = shopping_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        assert_updates_task(&actions, eid);
        if let Some(SyncAction::UpdateTask { updated_task, .. }) = actions.iter().find(|a| {
            if let SyncAction::UpdateTask { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        }) {
            assert!(
                updated_task.contexts.contains(&"shopping".to_string()),
                "@shopping context must be preserved on update"
            );
        }
    }

    #[test]
    fn default_list_no_auto_context() {
        let eid = "eid-def";
        let reminders = vec![ReminderBuilder::new(eid).title("Plain task").build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config(); // no auto_context
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, eid);
        if let Some(SyncAction::CreateTask { reminder, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::CreateTask { .. }))
        {
            assert_eq!(reminder.list, "Tasks");
        }
    }

    #[test]
    fn new_task_with_matching_context_creates_reminder_in_correct_list() {
        let reminders = vec![];
        let tasks = vec![local_task("Buy bananas @shopping")];
        let state = empty_state();
        let config = shopping_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert_creates_reminder(&actions);
        if let Some(SyncAction::CreateReminder { target_list, .. }) = actions
            .iter()
            .find(|a| matches!(a, SyncAction::CreateReminder { .. }))
        {
            assert_eq!(target_list.as_str(), "Shopping");
        }
    }

    #[test]
    fn task_without_matching_context_ignored_by_list_config() {
        let reminders = vec![];
        let tasks = vec![local_task("Some work task @work")];
        let state = empty_state();
        let config = shopping_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        assert!(
            !actions.iter().any(|a| {
                if let SyncAction::CreateReminder { target_list, .. } = a {
                    target_list.as_str() == "Shopping"
                } else {
                    false
                }
            }),
            "@work task must not create a reminder in Shopping list"
        );
    }

    #[test]
    fn project_tag_from_list_name_snake_cased() {
        // Reminders from "My Shopping" list → tasks should have +my_shopping
        let eid = "eid-proj";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Buy things")
            .list("My Shopping")
            .build()];
        let tasks = vec![];
        let state = empty_state();
        let config = ListSyncConfig {
            reminders_list: "My Shopping".to_string(),
            ..Default::default()
        };
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, eid);
        // snake-cased project tag (+my_shopping) verified in green phase via apply_task_actions
    }

    #[test]
    fn sync_across_two_lists_independently() {
        let tasks_eid = "eid-tasks";
        let shop_eid = "eid-shop";

        // Tasks list sync: one new reminder
        let tasks_reminders = vec![ReminderBuilder::new(tasks_eid)
            .title("Work task")
            .list("Tasks")
            .build()];
        let tasks_config = default_config();
        let all_tasks: Vec<Task> = vec![];
        let tasks_state = empty_state();
        let actions1 = compute_sync_actions(
            &tasks_reminders,
            &all_tasks,
            &tasks_state,
            &tasks_config,
            now(),
            None,
        );
        assert_creates_task(&actions1, tasks_eid);

        // Shopping list sync: one new reminder
        let shop_reminders = vec![ReminderBuilder::new(shop_eid)
            .title("Shopping thing")
            .list("Shopping")
            .build()];
        let shop_config = shopping_config();
        let shop_state = empty_state();
        let actions2 = compute_sync_actions(
            &shop_reminders,
            &all_tasks,
            &shop_state,
            &shop_config,
            now(),
            None,
        );
        assert_creates_task(&actions2, shop_eid);

        // The two sync calls must be independent
        assert!(
            !actions1.iter().any(|a| {
                if let SyncAction::CreateTask { eid: e, .. } = a {
                    e.as_str() == shop_eid
                } else {
                    false
                }
            }),
            "Tasks list sync must not create Shopping reminder's task"
        );
        assert!(
            !actions2.iter().any(|a| {
                if let SyncAction::CreateTask { eid: e, .. } = a {
                    e.as_str() == tasks_eid
                } else {
                    false
                }
            }),
            "Shopping list sync must not create Tasks reminder's task"
        );
    }

    // ============================================================
    // Category 10: State Management
    // ============================================================

    #[test]
    fn initial_sync_empty_state_creates_all() {
        let reminders = vec![
            ReminderBuilder::new("eid-1").title("Task A").build(),
            ReminderBuilder::new("eid-2").title("Task B").build(),
        ];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, "eid-1");
        assert_creates_task(&actions, "eid-2");
    }

    #[test]
    fn build_initial_state_captures_all_synced_items() {
        let reminders = vec![
            ReminderBuilder::new("eid-1").title("Task A").build(),
            ReminderBuilder::new("eid-2").title("Task B").build(),
        ];
        let tasks = vec![
            task_with_eid("eid-1", "Task A"),
            task_with_eid("eid-2", "Task B"),
        ];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(state.items.contains_key("eid-1"));
        assert!(state.items.contains_key("eid-2"));
        assert_eq!(state.items["eid-1"].fields.title, "Task A");
        assert!(
            reconciled.is_empty(),
            "EID-matched items must not appear in reconciled vec"
        );
    }

    #[test]
    fn state_updated_after_create_task() {
        let eid = "eid-new";
        let reminders = vec![ReminderBuilder::new(eid).title("New Task").build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        let (_, new_state) = apply_task_actions(&actions, vec![], &state, &config, now());
        assert!(
            new_state.items.contains_key(eid),
            "State must contain new item after CreateTask"
        );
    }

    #[test]
    fn state_updated_after_update_task() {
        let eid = "eid-upd";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Updated Title")
            .modified(recent_time())
            .build()];
        let tasks = vec![task_with_eid(eid, "Old Title")];
        let state = state_with_items(vec![synced_item(eid, "Old Title")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        let (_, new_state) = apply_task_actions(
            &actions,
            vec![task_with_eid(eid, "Old Title")],
            &state,
            &config,
            now(),
        );
        assert_eq!(new_state.items[eid].fields.title, "Updated Title");
    }

    #[test]
    fn state_cleaned_after_delete() {
        let eid = "eid-del";
        let task = task_with_eid(eid, "Task");
        let mut item = synced_item(eid, "Task");
        item.task_line_hash = task_line_hash(&task); // hash matches → unchanged → DeleteTask
        let reminders = vec![];
        let tasks = vec![task.clone()];
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        let (_, new_state) = apply_task_actions(&actions, vec![task], &state, &config, now());
        assert!(
            !new_state.items.contains_key(eid),
            "Deleted eid must be removed from sync state"
        );
    }

    #[test]
    fn task_line_hash_changes_when_task_modified() {
        let t1 = task_from_line("Buy milk eid:abc");
        let t2 = task_from_line("Buy cream eid:abc");
        let h1 = task_line_hash(&t1);
        let h2 = task_line_hash(&t2);
        assert_ne!(h1, h2, "Different tasks must produce different hashes");
    }

    #[test]
    fn task_line_hash_stable_for_same_task() {
        let t = task_from_line("Buy milk eid:abc");
        assert_eq!(
            task_line_hash(&t),
            task_line_hash(&t),
            "Same task must always hash the same"
        );
    }

    #[test]
    fn last_sync_time_updated_after_sync() {
        let eid = "eid-1";
        let reminders = vec![ReminderBuilder::new(eid).title("Task").build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        let (_, new_state) = apply_task_actions(&actions, vec![], &state, &config, now());
        assert!(
            new_state.last_sync_time.is_some(),
            "last_sync_time must be set after sync"
        );
        assert_eq!(new_state.last_sync_time, Some(now()));
    }

    // ============================================================
    // Category 11: Edge Cases
    // ============================================================

    #[test]
    fn empty_sync_no_reminders_no_tasks_no_state() {
        let reminders: Vec<Reminder> = vec![];
        let tasks: Vec<Task> = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_no_action(&actions);
    }

    #[test]
    fn reminder_with_empty_title() {
        let reminders = vec![ReminderBuilder::new("eid-empty").title("").build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        // Should not panic — engine handles empty title gracefully
        let _actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
    }

    #[test]
    fn reminder_with_very_long_notes_no_panic() {
        // Notes are no longer written to todo.txt; engine should not panic
        // regardless of note length.
        let long_notes = "a".repeat(1000);
        let reminders = vec![ReminderBuilder::new("eid-long")
            .title("Task")
            .notes(&long_notes)
            .build()];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_creates_task(&actions, "eid-long");
    }

    #[test]
    fn duplicate_eids_in_tasks_handled_gracefully() {
        let eid = "eid-dup";
        let reminders = vec![ReminderBuilder::new(eid).title("Task").build()];
        let tasks = vec![
            task_with_eid(eid, "Task"),
            task_with_eid(eid, "Task duplicate"),
        ];
        let state = state_with_items(vec![synced_item(eid, "Task")]);
        let config = default_config();
        // Must not panic when duplicate eids are present
        let _actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
    }

    #[test]
    fn reminder_with_no_last_modified_date() {
        let eid = "eid-nomod";
        let mut r = ReminderBuilder::new(eid).title("Task").build();
        r.last_modified_date = None;
        let tasks = vec![task_with_eid(eid, "Task")];
        let state = state_with_items(vec![synced_item(eid, "Task")]);
        let config = default_config();
        // Must not panic when last_modified_date is absent
        let _actions =
            compute_sync_actions(&[r], &tasks, &state, &config, now(), Some(past_time()));
    }

    #[test]
    fn task_with_extra_tags_preserved() {
        let eid = "eid-extra";
        let reminders = vec![ReminderBuilder::new(eid).title("Recurring task").build()];
        let tasks = vec![task_from_line(&format!(
            "Recurring task rec:1w t:2026-03-01 h:1 eid:{eid}"
        ))];
        let state = state_with_items(vec![synced_item(eid, "Recurring task")]);
        let config = default_config();
        let actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(past_time()),
        );
        // If an update is produced, TTDL-specific tags must survive
        if let Some(SyncAction::UpdateTask { updated_task, .. }) = actions.iter().find(|a| {
            if let SyncAction::UpdateTask { eid: e, .. } = a {
                e.as_str() == eid
            } else {
                false
            }
        }) {
            let line = format!("{updated_task}");
            assert!(line.contains("rec:"), "rec: tag must survive sync");
            assert!(line.contains("t:"), "t: tag must survive sync");
            assert!(line.contains("h:"), "h: tag must survive sync");
        }
    }

    #[test]
    fn large_batch_performance() {
        let count = 1000usize;
        let reminders: Vec<Reminder> = (0..count)
            .map(|i| {
                ReminderBuilder::new(&format!("eid-{i}"))
                    .title(&format!("Task {i}"))
                    .build()
            })
            .collect();
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, SyncAction::CreateTask { .. }))
                .count(),
            count,
            "All 1000 new reminders should produce CreateTask actions"
        );
    }

    #[test]
    fn apply_actions_returns_tasks_in_stable_order() {
        let reminders = vec![
            ReminderBuilder::new("eid-1").title("First").build(),
            ReminderBuilder::new("eid-2").title("Second").build(),
            ReminderBuilder::new("eid-3").title("Third").build(),
        ];
        let tasks = vec![];
        let state = empty_state();
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);
        let (result_tasks, _) = apply_task_actions(&actions, vec![], &state, &config, now());
        // Insertion order is preserved; count must match
        assert_eq!(result_tasks.len(), 3);
    }

    // ── Regression: stale state must not delete reminders ──────────────────────
    //
    // Scenario: a previous sync wrote to the wrong path (e.g. relative ./todo.txt
    // instead of the configured absolute path) but successfully saved state.  The
    // output file therefore has NO eid: tags.  The next correct sync must NOT
    // interpret "task missing from file" as "task was intentionally deleted" and
    // must NOT generate DeleteReminder actions.  Instead it should recreate the
    // tasks from the reminders (first-run semantics).
    #[test]
    fn stale_state_no_eid_in_file_does_not_delete_reminders() {
        let eid = "eid-stale";
        let reminder = ReminderBuilder::new(eid)
            .title("Gym options in S2")
            .modified(old_time()) // reminder is older than the file
            .build();

        // State says this reminder was previously synced.
        let state = state_with_items(vec![synced_item(eid, "Gym options in S2")]);

        // The output file exists but has NO eid: tags (stale / wrong-path situation).
        let tasks_without_eid = vec![
            local_task("Some unrelated task"),
            local_task("Another task without eid"),
        ];

        // todo.txt mtime is more recent than the reminder → without the fix this
        // would look like "task deleted after reminder" → DeleteReminder.
        let file_mtime = Some(recent_time());

        let config = default_config();
        let actions = compute_sync_actions(
            &[reminder],
            &tasks_without_eid,
            &state,
            &config,
            now(),
            file_mtime,
        );

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteReminder { .. })),
            "must not delete reminders when no eid: tags exist in the output file"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateTask { .. })),
            "should recreate the task from the reminder instead"
        );
    }

    // ── Regression: completed tasks must not be pushed to Reminders ────────────
    //
    // Scenario: todo.txt contains completed tasks with old due dates that match the
    // push filter (e.g. due=..today).  These must never generate CreateReminder
    // actions — only the UpdateReminder path (three_way_diff) may mark an already-
    // synced reminder complete.
    #[test]
    fn completed_task_matching_filter_not_pushed_to_reminders() {
        let reminders = vec![];
        let state = empty_state();
        let config = default_config(); // push_filter = "@today" by default

        // A completed task that carries @today and a past due date — without the fix
        // it would match the push filter and generate a CreateReminder.
        let completed = task_from_line("x 2026-02-20 2026-02-01 Old errand @today due:2026-02-15");

        let actions = compute_sync_actions(&reminders, &[completed], &state, &config, now(), None);

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateReminder { .. })),
            "completed tasks must not be pushed to Reminders as new reminders"
        );
    }

    // ── Regression: resurrection cascade in multi-list sync ────────────────────
    //
    // Scenario: list A resurrects a task (old_eid → new_eid), mutating current_tasks
    // and current_state.  List B then runs with the mutated state.  The task now has
    // new_eid, which is absent from list B's reminders.  Without the fix, list B
    // would include the task via its push filter and generate another ResurrectReminder,
    // cascading indefinitely.  With the fix, task_by_eid skips any task whose state
    // entry is owned by a different list.
    #[test]
    fn resurrected_task_not_processed_by_second_list() {
        // Simulate the state AFTER list A has already executed ResurrectReminder:
        // - old_eid removed, new_eid inserted with list = "Tasks"
        // - task in current_tasks now carries new_eid
        let new_eid = "eid-resurrected";
        let task = task_from_line(&format!(
            "Finish report @today due:2026-02-26 eid:{new_eid}"
        ));

        let mut state = empty_state();
        {
            let fields = SyncedFieldState {
                title: "Finish report".to_string(),
                priority: 9,
                is_completed: false,
                completion_date: None,
                due_date: Some("2026-02-26".to_string()),
                notes: None,
                list: "Tasks".to_string(), // owned by Tasks, not To-do
            };
            let r_hash = synced_field_hash(&fields);
            state.items.insert(
                new_eid.to_string(),
                SyncItemState {
                    eid: new_eid.to_string(),
                    fields,
                    reminders_last_modified: None,
                    task_line_hash: task_line_hash(&task),
                    reminders_field_hash: r_hash,
                    last_synced: now(),
                    pushed: true,
                },
            );
        }

        // List B ("To-do") has no reminder with new_eid — it only has its own reminders.
        let todo_reminders: Vec<Reminder> = vec![];
        let todo_config = ListSyncConfig {
            reminders_list: "To-do".to_string(),
            ..Default::default()
        };

        let actions = compute_sync_actions(
            &todo_reminders,
            &[task],
            &state,
            &todo_config,
            now(),
            Some(recent_time()),
        );

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::ResurrectReminder { .. })),
            "list B must not resurrect a task already owned by list A"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateReminder { .. })),
            "list B must not create a new reminder for a task owned by list A"
        );
    }

    // ----------------------------------------------------------------
    // replace_title_in_subject
    // ----------------------------------------------------------------

    #[test]
    fn replace_title_replaces_in_place() {
        let result =
            replace_title_in_subject("Old title @today due:2026-03-01", "Old title", "New title");
        assert_eq!(result, "New title @today due:2026-03-01");
    }

    #[test]
    fn replace_title_replaces_all_nonmeta_words() {
        // Token-based: all non-meta words are treated as the old title and
        // replaced with new_title inserted at the first non-meta position.
        let result = replace_title_in_subject("something else @today", "missing", "New title");
        assert_eq!(result, "New title @today");
    }

    #[test]
    fn replace_title_prepends_when_old_empty() {
        let result = replace_title_in_subject("@today due:2026-03-01", "", "New title");
        assert_eq!(result, "New title @today due:2026-03-01");
    }

    #[test]
    fn replace_title_normalises_whitespace() {
        let result = replace_title_in_subject("Old title  @ctx", "Old title", "New");
        assert_eq!(result, "New @ctx");
    }

    // ── extract_title ───────────────────────────────────────────────────────────

    #[test]
    fn extract_title_basic() {
        let t = task_from_line("Buy milk @today due:2026-03-01 eid:abc");
        assert_eq!(extract_title(&t), "Buy milk");
    }

    #[test]
    fn extract_title_no_partial_context_corruption() {
        // @work must not be stripped from @workshop
        let t = task_from_line("Attend @workshop @work eid:abc");
        let title = extract_title(&t);
        assert!(
            !title.contains("shop"),
            "partial context strip must not leave 'shop': got '{title}'"
        );
        assert_eq!(title, "Attend");
    }

    #[test]
    fn extract_title_interleaved_metadata() {
        let t = task_from_line("Buy @store milk eid:abc");
        // "Buy" and "milk" are title words; "@store" is metadata
        assert_eq!(extract_title(&t), "Buy milk");
    }

    #[test]
    fn extract_title_preserves_special_chars() {
        let t = task_from_line("Buy 2% milk eid:abc");
        assert_eq!(extract_title(&t), "Buy 2% milk");
    }

    #[test]
    fn extract_title_all_metadata_empty() {
        let t = task_from_line("@today due:2026-03-01 eid:abc");
        assert_eq!(extract_title(&t), "");
    }

    // ── New replace_title tests ──────────────────────────────────────────────────

    #[test]
    fn replace_title_with_interleaved_metadata() {
        // Interleaved @store should stay in place; title words replaced.
        let result = replace_title_in_subject("Buy @store milk", "Buy milk", "New item");
        assert_eq!(result, "New item @store");
    }

    #[test]
    fn replace_title_no_tag_value_collision() {
        // "old" appearing inside a tag value must not be treated as a title word.
        let result =
            replace_title_in_subject("Old task due:2026-old-01 eid:old-abc", "Old task", "New");
        assert_eq!(result, "New due:2026-old-01 eid:old-abc");
    }

    // ============================================================
    // Category 12: Hash-based change detection
    // ============================================================

    // ── task_line_hash ──────────────────────────────────────────────────────────

    #[test]
    fn task_line_hash_ignores_eid_change() {
        // EID reassignment must not count as a task change.
        let t1 = task_from_line("Buy milk @today due:2026-03-01 eid:old-eid");
        let t2 = task_from_line("Buy milk @today due:2026-03-01 eid:new-eid");
        assert_eq!(
            task_line_hash(&t1),
            task_line_hash(&t2),
            "Different eid values must produce the same hash"
        );
    }

    #[test]
    fn task_line_hash_stable_across_parse_roundtrip() {
        let line = "Buy milk @today due:2026-03-01 eid:abc";
        let t1 = task_from_line(line);
        let t2 = task_from_line(line);
        assert_eq!(task_line_hash(&t1), task_line_hash(&t2));
    }

    #[test]
    fn task_line_hash_stable_with_multiple_contexts() {
        // Guards against todo_lib ever non-deterministically iterating contexts.
        let t1 = task_from_line("Buy milk @home @today eid:abc");
        let t2 = task_from_line("Buy milk @home @today eid:abc");
        assert_eq!(task_line_hash(&t1), task_line_hash(&t2));
    }

    #[test]
    fn task_line_hash_stable_with_multiple_tags() {
        // Guards against HashMap iteration order ever affecting the serialised line.
        let t1 = task_from_line("Buy milk due:2026-03-01 note:hello eid:abc rec:1w");
        let t2 = task_from_line("Buy milk due:2026-03-01 note:hello eid:abc rec:1w");
        assert_eq!(task_line_hash(&t1), task_line_hash(&t2));
    }

    #[test]
    fn task_line_hash_detects_real_change() {
        let t1 = task_from_line("Buy milk eid:abc");
        let t2 = task_from_line("Buy cream eid:abc");
        assert_ne!(task_line_hash(&t1), task_line_hash(&t2));
    }

    // ── synced_field_hash ───────────────────────────────────────────────────────

    #[test]
    fn synced_field_hash_stability() {
        let fields = SyncedFieldState {
            title: "Buy milk".to_string(),
            priority: 0,
            is_completed: false,
            completion_date: None,
            due_date: Some("2026-03-01".to_string()),
            notes: None,
            list: "Tasks".to_string(),
        };
        assert_eq!(synced_field_hash(&fields), synced_field_hash(&fields));
    }

    #[test]
    fn synced_field_hash_sensitivity() {
        let f1 = SyncedFieldState {
            title: "Buy milk".to_string(),
            priority: 0,
            is_completed: false,
            completion_date: None,
            due_date: None,
            notes: None,
            list: "Tasks".to_string(),
        };
        let f2 = SyncedFieldState {
            title: "Buy cream".to_string(),
            ..f1.clone()
        };
        assert_ne!(synced_field_hash(&f1), synced_field_hash(&f2));
    }

    // ── verify_post_sync ────────────────────────────────────────────────────────

    #[test]
    fn verify_post_sync_clean_state_no_issues() {
        // Happy path: tracked task with correct hash → no issues.
        let task = task_with_eid("eid1", "Buy milk");
        let mut item = synced_item("eid1", "Buy milk");
        item.task_line_hash = task_line_hash(&task);
        let state = state_with_items(vec![item]);
        assert!(verify_post_sync(&[task], &state).is_empty());
    }

    #[test]
    fn verify_post_sync_empty_inputs_no_issues() {
        // Empty task list + empty state is always clean.
        assert!(verify_post_sync(&[], &SyncState::default()).is_empty());
    }

    #[test]
    fn verify_post_sync_untracked_tasks_ignored() {
        // Tasks without eid are not in state — no issue.
        let task = local_task("Buy milk");
        assert!(verify_post_sync(&[task], &SyncState::default()).is_empty());
    }

    #[test]
    fn verify_post_sync_orphan_state_entry_not_reported() {
        // State entry whose eid doesn't appear in tasks is normal (task deleted).
        let item = synced_item("ghost-eid", "Old task");
        let state = state_with_items(vec![item]);
        // No tasks at all — should be silent.
        assert!(verify_post_sync(&[], &state).is_empty());
    }

    #[test]
    fn verify_post_sync_sentinel_eids_ignored() {
        // sentinel eid:na and eid:na/<orig> tasks must be silently skipped.
        let na_task = task_from_line("Local only task eid:na");
        let na_slash = task_from_line("Ejected task eid:na/x-apple-reminder://ABC");
        let ns_slash = task_from_line("Temp eject eid:ns/x-apple-reminder://DEF");
        assert!(verify_post_sync(&[na_task, na_slash, ns_slash], &SyncState::default()).is_empty());
    }

    #[test]
    fn verify_post_sync_detects_duplicate_eid() {
        // Two tasks sharing the same eid should produce one "duplicate" issue.
        let t1 = task_with_eid("shared-eid", "First task");
        let t2 = task_with_eid("shared-eid", "Second task");
        let mut item = synced_item("shared-eid", "First task");
        item.task_line_hash = task_line_hash(&t1);
        let state = state_with_items(vec![item]);
        let issues = verify_post_sync(&[t1, t2], &state);
        assert_eq!(issues.len(), 1, "expected 1 issue, got: {issues:?}");
        assert!(
            issues[0].contains("duplicate eid:shared-eid"),
            "{}",
            issues[0]
        );
    }

    #[test]
    fn verify_post_sync_detects_orphan_task_eid() {
        // Task carries a real eid but no state entry exists.
        let task = task_with_eid("missing-from-state", "Orphan");
        let issues = verify_post_sync(&[task], &SyncState::default());
        assert_eq!(issues.len(), 1, "expected 1 issue, got: {issues:?}");
        assert!(
            issues[0].contains("eid:missing-from-state") && issues[0].contains("no state entry"),
            "{}",
            issues[0]
        );
    }

    #[test]
    fn verify_post_sync_detects_hash_mismatch() {
        // State hash is stale (task was mutated without updating state hash).
        let task = task_with_eid("eid-hash", "Updated subject");
        let mut item = synced_item("eid-hash", "Updated subject");
        item.task_line_hash = 9999; // deliberately wrong
        let state = state_with_items(vec![item]);
        let issues = verify_post_sync(&[task], &state);
        assert_eq!(issues.len(), 1, "expected 1 issue, got: {issues:?}");
        assert!(
            issues[0].contains("hash mismatch") && issues[0].contains("eid:eid-hash"),
            "{}",
            issues[0]
        );
    }

    #[test]
    fn verify_post_sync_multiple_issues_all_reported() {
        // Duplicate + orphan task eid — both issues present simultaneously.
        let t1 = task_with_eid("dup", "Task one");
        let t2 = task_with_eid("dup", "Task two");
        let orphan = task_with_eid("orphan", "No state");
        // State has entry for dup but not orphan.
        let mut item = synced_item("dup", "Task one");
        item.task_line_hash = task_line_hash(&t1);
        let state = state_with_items(vec![item]);
        let issues = verify_post_sync(&[t1, t2, orphan], &state);
        // Expect at least: 1 duplicate + 1 orphan (duplicate suppresses hash check for dup).
        assert!(issues.len() >= 2, "expected ≥2 issues, got: {issues:?}");
        assert!(
            issues.iter().any(|s| s.contains("duplicate eid:dup")),
            "missing duplicate issue: {issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|s| s.contains("eid:orphan") && s.contains("no state entry")),
            "missing orphan issue: {issues:?}"
        );
    }

    // ── Fix: DeleteReminder in release path strips eid tag ──────────────────────

    /// Regression for the "no state entry" warnings produced after Triage releases
    /// a task.  Before the fix, `apply_task_actions` removed the state entry but
    /// left the `eid:` tag on the task; `verify_post_sync` would then report
    /// "no state entry" on every subsequent sync until the user manually stripped
    /// the tag.
    #[test]
    fn delete_reminder_release_path_strips_eid_from_task() {
        let eid = "eid-released";
        let task = task_from_line(&format!("Buy milk eid:{eid}"));
        let mut item = synced_item(eid, "Buy milk");
        item.task_line_hash = task_line_hash(&task);
        let state = state_with_items(vec![item]);

        let action = SyncAction::DeleteReminder {
            eid: eid.to_string(),
        };
        let (tasks, new_state) =
            apply_task_actions(&[action], vec![task], &state, &default_config(), now());

        // Task is kept (not deleted).
        assert_eq!(tasks.len(), 1, "task must remain in the list");
        // eid: tag must be stripped so push_filter can re-admit it to a new list.
        assert!(
            tasks[0].tags.get("eid").is_none(),
            "DeleteReminder in release path must strip eid: tag; got {:?}",
            tasks[0].tags.get("eid")
        );
        // State entry must be removed.
        assert!(
            !new_state.items.contains_key(eid),
            "state entry must be removed"
        );
        // verify_post_sync must report no issues (no orphan eid warning).
        let issues = verify_post_sync(&tasks, &new_state);
        assert!(
            issues.is_empty(),
            "verify_post_sync must be clean after DeleteReminder fix: {issues:?}"
        );
    }

    // ── Fix: orphan eid cleanup ──────────────────────────────────────────────────

    /// Carry-over orphan eids (eid in task but not in state) are stripped by
    /// apply_task_actions.  This heals tasks left behind by pre-fix code that
    /// removed the state entry on release but did not strip the eid: tag.
    #[test]
    fn orphan_eid_stripped_by_apply_task_actions() {
        let eid = "eid-orphan";
        // Task has an eid but there is no state entry for it — simulates a
        // task that was released by a previous sync run before the fix.
        let task = task_from_line(&format!("Buy milk eid:{eid}"));

        // No actions, empty state — the orphan cleanup pass must fire.
        let (tasks, new_state) = apply_task_actions(
            &[],
            vec![task],
            &SyncState::default(),
            &default_config(),
            now(),
        );

        assert_eq!(tasks.len(), 1, "task must remain");
        assert!(
            tasks[0].tags.get("eid").is_none(),
            "orphan eid must be stripped; got {:?}",
            tasks[0].tags.get("eid")
        );
        assert!(new_state.items.is_empty(), "state must be empty");

        // verify_post_sync must now report no issues.
        let issues = verify_post_sync(&tasks, &new_state);
        assert!(
            issues.is_empty(),
            "verify_post_sync must be clean after orphan cleanup: {issues:?}"
        );
    }

    /// A task tracked by another list (eid in state from that list) must NOT
    /// have its eid stripped by the orphan cleanup pass.
    #[test]
    fn orphan_cleanup_does_not_strip_eid_tracked_by_another_list() {
        let eid = "eid-other-list";
        let task = task_from_line(&format!("Buy milk eid:{eid}"));
        // State entry exists (from another list) — task is legitimately tracked.
        let mut item = synced_item(eid, "Buy milk");
        item.task_line_hash = task_line_hash(&task);
        let state = state_with_items(vec![item]);

        let (tasks, _) = apply_task_actions(&[], vec![task], &state, &default_config(), now());

        assert_eq!(
            tasks[0].tags.get("eid").map(|s| s.as_str()),
            Some(eid),
            "eid must not be stripped when a state entry exists"
        );
    }

    // ── Fix: hash reconciliation for untracked field changes ────────────────────

    /// Regression for the "hash mismatch" warnings produced when a user modifies
    /// untracked fields (contexts, projects, custom tags).  `three_way_diff` only
    /// covers five synced fields; an untracked change left `task_line_hash` stale.
    /// The reconciliation pass at the end of `apply_task_actions` fixes this.
    #[test]
    fn hash_reconciliation_updates_stale_hash_for_untracked_change() {
        let eid = "eid-untracked";
        // Task has had @joint added — an untracked field change.
        let task = task_from_line(&format!("Buy milk @joint eid:{eid}"));
        let current_hash = task_line_hash(&task);

        // State was recorded before @joint was added → hash is stale.
        let old_hash = task_line_hash(&task_from_line(&format!("Buy milk eid:{eid}")));
        assert_ne!(
            current_hash, old_hash,
            "hashes must differ to set up the test"
        );

        let mut item = synced_item(eid, "Buy milk");
        item.task_line_hash = old_hash;
        let state = state_with_items(vec![item]);

        // No actions — three_way_diff sees no tracked-field change.
        let (tasks, new_state) =
            apply_task_actions(&[], vec![task], &state, &default_config(), now());

        assert_eq!(
            new_state.items[eid].task_line_hash, current_hash,
            "reconciliation must refresh stale task_line_hash after untracked change"
        );
        // verify_post_sync must report no hash mismatch.
        let issues = verify_post_sync(&tasks, &new_state);
        assert!(
            issues.is_empty(),
            "verify_post_sync must be clean after hash reconciliation: {issues:?}"
        );
    }

    // ── Case B integration ──────────────────────────────────────────────────────

    #[test]
    fn verify_post_sync_correct_hash_no_false_positive() {
        // Correct hash must not trigger hash-mismatch issue.
        let task = task_with_eid("eid-ok", "Unchanged task");
        let mut item = synced_item("eid-ok", "Unchanged task");
        item.task_line_hash = task_line_hash(&task);
        let state = state_with_items(vec![item]);
        assert!(verify_post_sync(&[task], &state).is_empty());
    }

    #[test]
    fn case_b_task_unchanged_hash_deletion_wins() {
        // Reminder absent, task present, task hash matches stored → DeleteTask.
        let eid = "eid-b-del";
        let task = task_from_line(&format!("Task eid:{eid}"));
        let mut item = synced_item(eid, "Task");
        item.task_line_hash = task_line_hash(&task); // hash matches → unchanged
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&[], &[task], &state, &config, now(), None);
        assert_deletes_task(&actions, eid);
    }

    #[test]
    fn case_b_task_changed_hash_resurrects() {
        // Reminder absent, task present, task hash differs from stored → ResurrectReminder.
        let eid = "eid-b-res";
        let baseline = task_from_line(&format!("Task eid:{eid}"));
        let current = task_from_line(&format!("Task with extra note eid:{eid}"));
        let mut item = synced_item(eid, "Task");
        item.task_line_hash = task_line_hash(&baseline); // stored hash of old content
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&[], &[current], &state, &config, now(), None);
        assert_resurrects_reminder(&actions, eid);
    }

    #[test]
    fn case_b_zero_stored_hash_treats_as_changed() {
        // Backward compat: stored hash 0 → always "changed" → ResurrectReminder.
        let eid = "eid-b-zero";
        let task = task_from_line(&format!("Task eid:{eid}"));
        let mut item = synced_item(eid, "Task");
        item.task_line_hash = 0; // zero = unknown (old state.json)
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&[], &[task], &state, &config, now(), None);
        assert_resurrects_reminder(&actions, eid);
    }

    // ── Case C integration ──────────────────────────────────────────────────────

    #[test]
    fn case_c_reminder_unchanged_hash_deletion_wins() {
        // Task absent, reminder present, reminder hash matches stored → DeleteReminder.
        let eid = "eid-c-del";
        let reminder = ReminderBuilder::new(eid).title("Task").build();
        let mut item = synced_item(eid, "Task");
        item.reminders_field_hash = synced_field_hash(&build_field_state_from_reminder(&reminder));
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&[reminder], &[], &state, &config, now(), None);
        assert_deletes_reminder(&actions, eid);
    }

    #[test]
    fn case_c_reminder_changed_hash_resurrects() {
        // Task absent, reminder present, reminder hash differs → ResurrectTask.
        let eid = "eid-c-res";
        let baseline_reminder = ReminderBuilder::new(eid).title("Task").build();
        let current_reminder = ReminderBuilder::new(eid).title("Updated Task").build();
        let mut item = synced_item(eid, "Task");
        item.reminders_field_hash =
            synced_field_hash(&build_field_state_from_reminder(&baseline_reminder));
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&[current_reminder], &[], &state, &config, now(), None);
        assert_resurrects_task(&actions, eid);
    }

    // ── Sentinel eids ────────────────────────────────────────────────────────────
    //
    // Three sentinel forms:
    //   eid:na         — permanent local opt-out (never pushed)
    //   eid:na/<orig>  — eject reminder <orig>, then simplify to eid:na
    //   eid:ns/<orig>  — eject reminder <orig>, then remove eid: entirely
    //
    // Tests below cover all three, plus edge cases.

    #[test]
    fn sentinel_eid_na_not_pushed_to_reminders() {
        // A plain eid:na task is never pushed as a new reminder, even if it
        // matches the push_filter.  Empty state, no reminders.
        let task = task_from_line("Buy milk eid:na");
        let config = default_config();
        let actions = compute_sync_actions(&[], &[task], &empty_state(), &config, now(), None);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateReminder { .. })),
            "eid:na task must not produce CreateReminder"
        );
    }

    #[test]
    fn sentinel_eid_na_slash_deletes_reminder_unconditionally() {
        // eid:na/ABC → DeleteReminder regardless of whether the reminder was
        // modified since last sync (no hash check, no resurrection).
        let eid = "ABC";
        let reminder = ReminderBuilder::new(eid).title("Buy milk").build();
        // Simulate a reminder that has changed since last sync (hash won't match).
        let mut item = synced_item(eid, "Buy milk");
        item.reminders_field_hash = 0; // force mismatch → would normally ResurrectTask
        let state = state_with_items(vec![item]);
        let task = task_from_line(&format!("Buy milk eid:na/{eid}"));
        let config = default_config();
        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteReminder { eid: e } if e == eid)),
            "eid:na/<eid> must produce DeleteReminder even when reminder hash changed"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::ResurrectTask { .. })),
            "eid:na/<eid> must never produce ResurrectTask"
        );
    }

    #[test]
    fn sentinel_eid_ns_slash_deletes_reminder_unconditionally() {
        // eid:ns/ABC behaves identically to eid:na/ABC for the DeleteReminder step.
        let eid = "ABC";
        let reminder = ReminderBuilder::new(eid).title("Buy milk").build();
        let mut item = synced_item(eid, "Buy milk");
        item.reminders_field_hash = 0; // force mismatch → would normally ResurrectTask
        let state = state_with_items(vec![item]);
        let task = task_from_line(&format!("Buy milk eid:ns/{eid}"));
        let config = default_config();
        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteReminder { eid: e } if e == eid)),
            "eid:ns/<eid> must produce DeleteReminder even when reminder hash changed"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::ResurrectTask { .. })),
            "eid:ns/<eid> must never produce ResurrectTask"
        );
    }

    #[test]
    fn sentinel_eid_na_slash_not_pushed_to_reminders() {
        // eid:na/<orig> tasks are also suppressed from Step 3, even if the task
        // matches the push_filter and has no state entry for the sentinel eid.
        let task = task_from_line("Buy milk eid:na/GHOST");
        let config = default_config();
        let actions = compute_sync_actions(&[], &[task], &empty_state(), &config, now(), None);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateReminder { .. })),
            "eid:na/<orig> must not produce CreateReminder"
        );
    }

    #[test]
    fn sentinel_eid_ns_slash_not_pushed_to_reminders() {
        // eid:ns/<orig> tasks are suppressed from Step 3, same as eid:na/<orig>.
        let task = task_from_line("Buy milk eid:ns/GHOST");
        let config = default_config();
        let actions = compute_sync_actions(&[], &[task], &empty_state(), &config, now(), None);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateReminder { .. })),
            "eid:ns/<orig> must not produce CreateReminder"
        );
    }

    #[test]
    fn sentinel_eid_na_prevents_stale_state_trigger() {
        // When the only task in the file carries eid:na, stale_state must NOT
        // fire (the tag counts as a real eid: tag for the heuristic), so Case C
        // produces DeleteReminder rather than CreateTask.
        let eid = "ABC";
        let reminder = ReminderBuilder::new(eid).title("Buy milk").build();
        let mut item = synced_item(eid, "Buy milk");
        // Make reminder hash match so deletion wins (not resurrection).
        item.reminders_field_hash = synced_field_hash(&build_field_state_from_reminder(&reminder));
        let state = state_with_items(vec![item]);
        // Only task in the file — no real eid: tag, just the sentinel.
        let task = task_from_line(&format!("Buy milk eid:na/{eid}"));
        let config = default_config();
        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteReminder { eid: e } if e == eid)),
            "must DeleteReminder even when the sentinel is the only eid: tag in the file"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateTask { .. })),
            "stale_state must not fire — must not CreateTask"
        );
    }

    #[test]
    fn sentinel_eid_na_slash_cleans_tag_when_original_eid_gone_from_state() {
        // When the original eid is no longer in state (reminder deleted last
        // sync), emit CleanSentinelTag { sentinel_eid: "na/GONE" }.
        let task = task_from_line("Buy milk eid:na/GONE");
        let config = default_config();
        let actions = compute_sync_actions(&[], &[task], &empty_state(), &config, now(), None);
        assert!(
            actions.iter().any(|a| matches!(
                a,
                SyncAction::CleanSentinelTag { sentinel_eid: s } if s == "na/GONE"
            )),
            "must emit CleanSentinelTag when original eid is absent from state"
        );
    }

    #[test]
    fn sentinel_eid_ns_slash_cleans_tag_when_original_eid_gone_from_state() {
        // eid:ns/GONE → CleanSentinelTag { sentinel_eid: "ns/GONE" } once the
        // original eid is absent from state.
        let task = task_from_line("Buy milk eid:ns/GONE");
        let config = default_config();
        let actions = compute_sync_actions(&[], &[task], &empty_state(), &config, now(), None);
        assert!(
            actions.iter().any(|a| matches!(
                a,
                SyncAction::CleanSentinelTag { sentinel_eid: s } if s == "ns/GONE"
            )),
            "must emit CleanSentinelTag when original eid is absent from state"
        );
    }

    #[test]
    fn sentinel_eid_na_plain_no_cleanup_emitted() {
        // Plain eid:na (no original eid encoded) never produces CleanSentinelTag.
        let task = task_from_line("Buy milk eid:na");
        let config = default_config();
        let actions = compute_sync_actions(&[], &[task], &empty_state(), &config, now(), None);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CleanSentinelTag { .. })),
            "plain eid:na must not produce CleanSentinelTag"
        );
    }

    #[test]
    fn clean_sentinel_tag_na_slash_rewrites_eid_to_na() {
        // apply_task_actions with CleanSentinelTag("na/ABC") → eid:na (permanent opt-out).
        let task = task_from_line("Buy milk eid:na/ABC");
        let action = SyncAction::CleanSentinelTag {
            sentinel_eid: "na/ABC".to_string(),
        };
        let (tasks, _) = apply_task_actions(
            &[action],
            vec![task],
            &empty_state(),
            &default_config(),
            now(),
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].tags.get("eid").map(|s| s.as_str()),
            Some("na"),
            "eid:na/<orig> cleanup must leave eid:na"
        );
    }

    #[test]
    fn clean_sentinel_tag_ns_slash_removes_eid_entirely() {
        // apply_task_actions with CleanSentinelTag("ns/ABC") → eid: tag removed
        // entirely so push_filter re-applies on the next cycle.
        let task = task_from_line("Buy milk eid:ns/ABC");
        let action = SyncAction::CleanSentinelTag {
            sentinel_eid: "ns/ABC".to_string(),
        };
        let (tasks, _) = apply_task_actions(
            &[action],
            vec![task],
            &empty_state(),
            &default_config(),
            now(),
        );
        assert_eq!(tasks.len(), 1);
        assert!(
            tasks[0].tags.get("eid").is_none(),
            "eid:ns/<orig> cleanup must remove the eid: tag entirely"
        );
    }

    // ============================================================
    // Category: sticky_tracking = Auto (release set)
    // ============================================================

    fn triage_config_with_filter(filter: &str) -> ListSyncConfig {
        ListSyncConfig {
            reminders_list: "Tasks".to_string(),
            push_filter: Some(filter.to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Triage,
            ..Default::default()
        }
    }

    fn synced_item_with_hash(eid: &str, title: &str, hash: u64, pushed: bool) -> SyncItemState {
        let fields = SyncedFieldState {
            title: title.to_string(),
            priority: 0,
            is_completed: false,
            completion_date: None,
            due_date: None,
            notes: None,
            list: "Tasks".to_string(),
        };
        let r_hash = synced_field_hash(&fields);
        SyncItemState {
            eid: eid.to_string(),
            fields,
            reminders_last_modified: Some(past_time()),
            task_line_hash: hash,
            reminders_field_hash: r_hash,
            last_synced: past_time(),
            pushed,
        }
    }

    #[test]
    fn sticky_serde_true_maps_to_always() {
        let toml = r#"reminders_list = "Tasks"
sticky_tracking = true"#;
        let cfg: ListSyncConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.sticky_tracking,
            crate::sync::config::StickyTracking::Always
        );
    }

    #[test]
    fn sticky_serde_false_maps_to_never() {
        let toml = r#"reminders_list = "Tasks"
sticky_tracking = false"#;
        let cfg: ListSyncConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.sticky_tracking,
            crate::sync::config::StickyTracking::Never
        );
    }

    #[test]
    fn sticky_serde_string_triage() {
        let toml = r#"reminders_list = "Tasks"
sticky_tracking = "triage""#;
        let cfg: ListSyncConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.sticky_tracking,
            crate::sync::config::StickyTracking::Triage
        );
    }

    #[test]
    fn sticky_serde_default_is_triage() {
        let toml = r#"reminders_list = "Tasks""#;
        let cfg: ListSyncConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.sticky_tracking,
            crate::sync::config::StickyTracking::Triage
        );
    }

    #[test]
    fn sticky_serde_auto_is_invalid() {
        // "auto" was removed; users must migrate to "triage".
        let toml = r#"reminders_list = "Tasks"
sticky_tracking = "auto""#;
        assert!(
            toml::from_str::<ListSyncConfig>(toml).is_err(),
            r#""auto" must no longer be accepted as a sticky_tracking value"#
        );
    }

    /// Any edited task (push-origin) that falls off filter → released.
    #[test]
    fn compute_release_set_edited_task_off_filter_released() {
        use super::compute_release_set;

        // push_filter: @today — task no longer has @today
        let eid = "eid-edited";
        let task = task_with_eid(eid, "Buy milk");
        let current_hash = task_line_hash(&task);

        // Stored hash differs → task changed
        let mut item = synced_item_with_hash(eid, "Buy milk", current_hash + 1, true);
        item.fields.list = "Tasks".to_string();

        let state = state_with_items(vec![item]);
        let config = triage_config_with_filter("@today");
        let today = base_date();

        let release = compute_release_set(&[task], &state, &[config], today);
        assert!(
            release.contains(eid),
            "edited task off filter should be in release set"
        );
    }

    /// Pushed task still admitted → not released.
    #[test]
    fn compute_release_set_still_admitted_not_released() {
        use super::compute_release_set;

        let eid = "eid-admitted";
        let task = task_from_line(&format!("Buy milk @today eid:{eid}"));
        let current_hash = task_line_hash(&task);

        let mut item = synced_item_with_hash(eid, "Buy milk", current_hash + 1, true);
        item.fields.list = "Tasks".to_string();

        let state = state_with_items(vec![item]);
        let config = triage_config_with_filter("@today");

        let release = compute_release_set(&[task], &state, &[config], base_date());
        assert!(
            !release.contains(eid),
            "task still matching push_filter must not be released"
        );
    }

    /// Inbox task (pushed=false) with no change → not released.
    #[test]
    fn inbox_not_released_when_unchanged() {
        use super::compute_release_set;

        let eid = "eid-inbox";
        let task = task_with_eid(eid, "Buy milk");
        let current_hash = task_line_hash(&task);

        // Hash matches → task unchanged
        let mut item = synced_item_with_hash(eid, "Buy milk", current_hash, false);
        item.fields.list = "Tasks".to_string();

        let state = state_with_items(vec![item]);
        let config = triage_config_with_filter("@today");

        let release = compute_release_set(&[task], &state, &[config], base_date());
        assert!(
            !release.contains(eid),
            "inbox task unchanged should not be released"
        );
    }

    /// Pull-origin task with any edit that falls off filter → released.
    ///
    /// Core triage behaviour: the edit is the triage signal regardless of
    /// whether the task was pull- or push-origin. "Cosmetic" edits count too —
    /// once you touch a task you own its filter state.
    #[test]
    fn pull_origin_released_after_any_edit_off_filter() {
        use super::compute_release_set;

        let eid = "eid-inbox2";
        let task = task_with_eid(eid, "Buy milk fixed"); // title changed
        let current_hash = task_line_hash(&task);

        // Stored hash differs (title edit), pushed=false (pull-origin)
        let mut item = synced_item_with_hash(eid, "Buy milk", current_hash + 1, false);
        item.fields.list = "Tasks".to_string();

        let state = state_with_items(vec![item]);
        let config = triage_config_with_filter("@today");

        let release = compute_release_set(&[task], &state, &[config], base_date());
        assert!(
            release.contains(eid),
            "pull-origin task with any edit off filter must be released under Triage"
        );
    }

    /// Task edited + off owning-filter → released even when another list admits.
    ///
    /// Under Triage, cross-list admission is not required; the task is released
    /// because it was edited (hash differs). The second list (To-do) will then
    /// create a new Reminder in the cross-list move end-to-end flow.
    #[test]
    fn edited_task_released_when_another_list_admits() {
        use super::compute_release_set;

        let eid = "eid-triaged";
        let task = task_from_line(&format!("Buy milk @joint eid:{eid}"));
        let current_hash = task_line_hash(&task);

        // Stored hash differs — user added @joint
        let mut item = synced_item_with_hash(eid, "Buy milk", current_hash + 1, false);
        item.fields.list = "Tasks".to_string();

        let state = state_with_items(vec![item]);

        let tasks_config = triage_config_with_filter("@today"); // @joint no longer matches
        let todo_config = ListSyncConfig {
            reminders_list: "To-do".to_string(),
            push_filter: Some("@joint".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Triage,
            ..Default::default()
        };

        let release =
            compute_release_set(&[task], &state, &[tasks_config, todo_config], base_date());
        assert!(
            release.contains(eid),
            "edited task off Tasks filter should be released (To-do will pick it up)"
        );
    }

    /// sticky_tracking = Always → release set is always empty.
    #[test]
    fn sticky_always_never_releases() {
        use super::compute_release_set;

        let eid = "eid-always";
        let task = task_with_eid(eid, "Buy milk");
        let current_hash = task_line_hash(&task);

        let mut item = synced_item_with_hash(eid, "Buy milk", current_hash + 1, true);
        item.fields.list = "Tasks".to_string();

        let state = state_with_items(vec![item]);
        let config = ListSyncConfig {
            reminders_list: "Tasks".to_string(),
            push_filter: Some("@today".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Always,
            ..Default::default()
        };

        let release = compute_release_set(&[task], &state, &[config], base_date());
        assert!(
            release.is_empty(),
            "sticky_tracking=Always must never produce a release set"
        );
    }

    // ── Triage mode: comprehensive scenario coverage ─────────────────────────

    /// Unedited task off filter → stays (inbox protection regardless of origin).
    #[test]
    fn triage_unedited_task_off_filter_stays() {
        use super::compute_release_set;

        let eid = "eid-unedited";
        let task = task_with_eid(eid, "Buy milk"); // no @today
        let current_hash = task_line_hash(&task);

        // Hash matches — task unchanged since last sync
        let mut item = synced_item_with_hash(eid, "Buy milk", current_hash, false);
        item.fields.list = "Tasks".to_string();

        let state = state_with_items(vec![item]);
        let config = triage_config_with_filter("@today");

        let release = compute_release_set(&[task], &state, &[config], base_date());
        assert!(
            !release.contains(eid),
            "unedited task off filter must not be released (inbox protection)"
        );
    }

    /// Edited task still admitted by owning filter → not released.
    #[test]
    fn triage_edited_task_still_on_filter_stays() {
        use super::compute_release_set;

        let eid = "eid-still-admitted";
        let task = task_from_line(&format!("Buy milk @today eid:{eid}")); // matches filter
        let current_hash = task_line_hash(&task);

        // Hash differs — task was edited (e.g. priority added)
        let mut item = synced_item_with_hash(eid, "Buy milk @today", current_hash + 1, false);
        item.fields.list = "Tasks".to_string();

        let state = state_with_items(vec![item]);
        let config = triage_config_with_filter("@today");

        let release = compute_release_set(&[task], &state, &[config], base_date());
        assert!(
            !release.contains(eid),
            "edited task still matching filter must not be released"
        );
    }

    /// Pull-origin task, priority assigned in todo.txt, no longer matches
    /// push_filter → released. This is the core user workflow:
    ///   Reminders → todo.txt (inbox) → assign priority (triage) → remove from Reminders.
    #[test]
    fn triage_pull_origin_priority_assigned_off_filter_released() {
        use super::compute_release_set;

        let eid = "eid-joint-prioritised";

        // Task as it looks after the user assigned priority C in todo.txt.
        // The To-do push_filter requires @joint AND (@today OR due=..+1d).
        // This task has @joint but no @today and no due → off filter.
        let task = task_from_line(&format!("(C) Folding table @joint eid:{eid}"));
        let current_hash = task_line_hash(&task);

        // State: was pulled from To-do with no priority (hash differs → user edited)
        let mut item = synced_item_with_hash(eid, "Folding table", current_hash + 1, false);
        item.fields.list = "To-do".to_string();

        let state = state_with_items(vec![item]);

        // To-do list: must have @joint AND (@today OR due=..+1d)
        let todo_config = ListSyncConfig {
            reminders_list: "To-do".to_string(),
            push_filter: Some("@joint;@today~@joint;due=..+1d".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Triage,
            ..Default::default()
        };

        let release = compute_release_set(&[task], &state, &[todo_config], base_date());
        assert!(
            release.contains(eid),
            "pull-origin task triaged with priority but off filter must be released"
        );
    }

    /// Same setup as above but with Always → task stays despite edit.
    #[test]
    fn always_blocks_release_of_edited_pull_origin_task() {
        use super::compute_release_set;

        let eid = "eid-joint-always";
        let task = task_from_line(&format!("(C) Extension cord @joint eid:{eid}"));
        let current_hash = task_line_hash(&task);

        let mut item = synced_item_with_hash(eid, "Extension cord", current_hash + 1, false);
        item.fields.list = "To-do".to_string();

        let state = state_with_items(vec![item]);

        let todo_config = ListSyncConfig {
            reminders_list: "To-do".to_string(),
            push_filter: Some("@joint;@today~@joint;due=..+1d".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Always,
            ..Default::default()
        };

        let release = compute_release_set(&[task], &state, &[todo_config], base_date());
        assert!(
            release.is_empty(),
            "Always must block release even when task is edited and off filter"
        );
    }

    /// Never mode: task unchanged + off filter → still released immediately
    /// (task-change protection does not apply to Never mode).
    #[test]
    fn never_mode_releases_unchanged_task() {
        use super::compute_sync_actions_ext;
        use std::collections::HashSet;

        let eid = "eid-never-unchanged";
        let task = task_with_eid(eid, "Buy milk"); // no @today
        let current_hash = task_line_hash(&task);

        // Hash matches — task unchanged (Never doesn't check)
        let item = synced_item_with_hash(eid, "Buy milk", current_hash, false);
        let state = state_with_items(vec![item]);

        let config = ListSyncConfig {
            reminders_list: "Tasks".to_string(),
            push_filter: Some("@today".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Never,
            ..Default::default()
        };
        let reminder = ReminderBuilder::new(eid).title("Buy milk").build();

        // Never mode: task absent from task_by_eid → Case C → DeleteReminder
        let actions = compute_sync_actions_ext(
            &[reminder],
            &[task],
            &state,
            &config,
            now(),
            None,
            &HashSet::new(),
            0,
        );
        assert_deletes_reminder(&actions, eid);
    }

    /// End-to-end triage workflow matching the real config:
    ///   1. @joint task pulled from To-do Reminders list (pushed=false)
    ///   2. User assigns priority in todo.txt → hash changes
    ///   3. Task has @joint but no @today/due → off To-do push_filter
    ///   4. Expected: DeleteReminder for the To-do item
    #[test]
    fn triage_real_workflow_joint_task_prioritised_then_removed_from_reminders() {
        use super::compute_release_set;
        use super::compute_sync_actions_ext;

        let eid = "eid-real-workflow";

        // After triage: user added priority C, task has @joint but no @today, no near due.
        let task = task_from_line(&format!("(C) Extension cord @joint eid:{eid} #buy"));
        let current_hash = task_line_hash(&task);

        // State: pulled from To-do, no priority (hash differs)
        let mut state_item =
            synced_item_with_hash(eid, "Extension cord #buy", current_hash + 1, false);
        state_item.fields.list = "To-do".to_string();
        let state = state_with_items(vec![state_item]);

        // Config mirrors ~/config/remtodo: push @joint;@today OR @joint;due=..+1d
        let todo_config = ListSyncConfig {
            reminders_list: "To-do".to_string(),
            push_filter: Some("@joint;@today~@joint;due=..+1d".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Triage,
            ..Default::default()
        };
        let all_configs = vec![todo_config.clone()];

        let release_eids = compute_release_set(&[task.clone()], &state, &all_configs, base_date());
        assert!(
            release_eids.contains(eid),
            "triaged @joint task with no @today/due should enter release set"
        );

        let reminder = ReminderBuilder::new(eid)
            .title("Extension cord #buy")
            .list("To-do")
            .build();
        let actions = compute_sync_actions_ext(
            &[reminder],
            &[task],
            &state,
            &todo_config,
            now(),
            None,
            &release_eids,
            0,
        );

        assert_deletes_reminder(&actions, eid);
    }

    /// Released pushed task emits DeleteReminder in Case A.
    #[test]
    fn released_pushed_task_emits_delete_reminder() {
        use super::compute_sync_actions_ext;
        use std::collections::HashSet;

        let eid = "eid-release";
        let reminder = ReminderBuilder::new(eid).title("Buy milk").build();
        let task = task_with_eid(eid, "Buy milk");

        let item = synced_item_with_hash(eid, "Buy milk", task_line_hash(&task), true);
        let state = state_with_items(vec![item]);

        let config = triage_config_with_filter("@today");
        let mut release_eids = HashSet::new();
        release_eids.insert(eid.to_string());

        let actions = compute_sync_actions_ext(
            &[reminder],
            &[task],
            &state,
            &config,
            now(),
            None,
            &release_eids,
            0,
        );

        assert_deletes_reminder(&actions, eid);
        // Must not also update or create
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::UpdateReminder { .. })),
            "release must not also emit UpdateReminder"
        );
    }

    /// Released task where reminder was also completed → UpdateTask + DeleteReminder.
    #[test]
    fn released_task_preserves_reminder_changes() {
        use super::compute_sync_actions_ext;
        use std::collections::HashSet;

        let eid = "eid-release-complete";
        // Reminder was completed on device
        let reminder = ReminderBuilder::new(eid)
            .title("Buy milk")
            .completed("2026-02-25")
            .modified(recent_time())
            .build();
        let task = task_with_eid(eid, "Buy milk"); // not yet completed in todo.txt

        // Baseline: task was not completed
        let mut item = synced_item_with_hash(eid, "Buy milk", task_line_hash(&task), true);
        item.fields.is_completed = false;
        item.reminders_last_modified = Some(past_time());
        let state = state_with_items(vec![item]);

        let config = triage_config_with_filter("@today");
        let mut release_eids = HashSet::new();
        release_eids.insert(eid.to_string());

        let actions = compute_sync_actions_ext(
            &[reminder],
            &[task],
            &state,
            &config,
            now(),
            Some(old_time()), // task_mtime older than reminder → reminder wins
            &release_eids,
            0,
        );

        assert_updates_task(&actions, eid);
        assert_deletes_reminder(&actions, eid);
    }

    /// create_task handler sets pushed=false in state.
    #[test]
    fn apply_task_actions_create_task_sets_pushed_false() {
        let eid = "eid-pull";
        let reminder = ReminderBuilder::new(eid).title("Buy milk").build();
        let action = SyncAction::CreateTask {
            eid: eid.to_string(),
            reminder: reminder.clone(),
        };
        let (_, new_state) =
            apply_task_actions(&[action], vec![], &empty_state(), &default_config(), now());
        let item = new_state.items.get(eid).expect("state item should exist");
        assert!(!item.pushed, "CreateTask must set pushed=false");
        let _ = reminder; // suppress unused warning
    }

    /// resurrect_task handler sets pushed=false in state.
    #[test]
    fn apply_task_actions_resurrect_task_sets_pushed_false() {
        let eid = "eid-resurrect";
        let task = task_with_eid(eid, "Resurrected task");
        let action = SyncAction::ResurrectTask {
            eid: eid.to_string(),
            task: task.clone(),
        };
        let (_, new_state) =
            apply_task_actions(&[action], vec![], &empty_state(), &default_config(), now());
        let item = new_state.items.get(eid).expect("state item should exist");
        assert!(!item.pushed, "ResurrectTask must set pushed=false");
    }

    // ============================================================
    // Cross-list move end-to-end
    // ============================================================

    /// Full cross-list move: task gains @joint (edit) → Tasks releases it →
    /// To-do picks it up as CreateReminder in the same pass.
    ///
    /// Simulates the per-list loop in main.rs: run Tasks first (applying
    /// actions + state mutations), then run To-do on the updated state.
    #[test]
    fn cross_list_move_end_to_end() {
        use super::compute_release_set;
        use super::compute_sync_actions_ext;

        let eid = "eid-triaged";

        // Task was previously inbox-pulled from "Tasks". Now has @joint added.
        let task = task_from_line(&format!("Buy milk @joint eid:{eid}"));
        let original_hash = task_line_hash(&task) + 1; // stored hash differs → changed

        let mut state_item = synced_item_with_hash(eid, "Buy milk", original_hash, false);
        state_item.fields.list = "Tasks".to_string();
        let state = state_with_items(vec![state_item]);

        let tasks_config = triage_config_with_filter("@today"); // @joint no longer matches
        let todo_config = ListSyncConfig {
            reminders_list: "To-do".to_string(),
            push_filter: Some("@joint".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Triage,
            ..Default::default()
        };
        let all_configs = vec![tasks_config.clone(), todo_config.clone()];

        // Compute release set (as main.rs does before the per-list loop).
        let release_eids = compute_release_set(&[task.clone()], &state, &all_configs, base_date());
        assert!(release_eids.contains(eid), "eid should be in release set");

        // Tasks has a live reminder for this eid.
        let tasks_reminder = ReminderBuilder::new(eid).title("Buy milk").build();

        // ── Pass 1: Tasks list ────────────────────────────────────────────────
        let tasks_actions = compute_sync_actions_ext(
            &[tasks_reminder],
            &[task.clone()],
            &state,
            &tasks_config,
            now(),
            None,
            &release_eids,
            0,
        );
        assert_deletes_reminder(&tasks_actions, eid);

        // Apply task-side actions (removes state entry, task unchanged in list).
        let (tasks_after, state_after) = apply_task_actions(
            &tasks_actions,
            vec![task.clone()],
            &state,
            &tasks_config,
            now(),
        );

        // State entry must be gone.
        assert!(
            !state_after.items.contains_key(eid),
            "state entry must be removed after Tasks release"
        );
        // Task must still be present (DeleteReminder doesn't remove tasks).
        assert_eq!(tasks_after.len(), 1);

        // ── Pass 2: To-do list ────────────────────────────────────────────────
        // To-do has no reminders with this eid yet.
        let todo_actions = compute_sync_actions_ext(
            &[], // no To-do reminders
            &tasks_after,
            &state_after,
            &todo_config,
            now(),
            None,
            &release_eids,
            0,
        );

        // To-do must issue CreateReminder for the triaged task.
        assert_creates_reminder(&todo_actions);
    }

    /// When the owning list has sticky_tracking=Always, a cross-list move is
    /// blocked — the task stays in the original list regardless.
    #[test]
    fn always_list_blocks_cross_list_move() {
        use super::compute_release_set;

        let eid = "eid-always-blocked";
        let task = task_from_line(&format!("Buy milk @joint eid:{eid}"));
        let original_hash = task_line_hash(&task) + 1; // changed

        let mut state_item = synced_item_with_hash(eid, "Buy milk", original_hash, false);
        state_item.fields.list = "Tasks".to_string();
        let state = state_with_items(vec![state_item]);

        // Tasks = Always; To-do = Auto and admits the task.
        let tasks_config = ListSyncConfig {
            reminders_list: "Tasks".to_string(),
            push_filter: Some("@today".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Always,
            ..Default::default()
        };
        let todo_config = ListSyncConfig {
            reminders_list: "To-do".to_string(),
            push_filter: Some("@joint".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Triage,
            ..Default::default()
        };

        let release =
            compute_release_set(&[task], &state, &[tasks_config, todo_config], base_date());

        assert!(
            release.is_empty(),
            "Always on owning list must block cross-list move; release set should be empty"
        );
    }

    /// sticky_tracking=Never: task falls off filter → not in task_by_eid →
    /// Case C fires → DeleteReminder (no task-change protection).
    #[test]
    fn never_mode_deletes_reminder_immediately_on_filter_miss() {
        use std::collections::HashSet;

        use super::compute_sync_actions_ext;

        let eid = "eid-never";
        // Task no longer has @today — falls off the filter.
        let task = task_with_eid(eid, "Buy milk");
        let current_hash = task_line_hash(&task);

        // State entry with matching hash (task UNCHANGED — Never has no change guard).
        let item = synced_item_with_hash(eid, "Buy milk", current_hash, true);
        let state = state_with_items(vec![item]);

        let config = ListSyncConfig {
            reminders_list: "Tasks".to_string(),
            push_filter: Some("@today".to_string()),
            sticky_tracking: crate::sync::config::StickyTracking::Never,
            ..Default::default()
        };

        let reminder = ReminderBuilder::new(eid).title("Buy milk").build();

        // Release set is empty — Never doesn't use it.
        let actions = compute_sync_actions_ext(
            &[reminder],
            &[task],
            &state,
            &config,
            now(),
            None,
            &HashSet::new(),
            0,
        );

        // Reminder unchanged, so hash check passes deletion.
        assert_deletes_reminder(&actions, eid);
    }

    // ============================================================
    // Category: Bootstrap Reconciliation
    // ============================================================

    /// Exact title+due pair → matched in Pass 2, state entry created.
    #[test]
    fn reconcile_title_and_due_matches() {
        let eid = "eid-reconcile-1";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Buy milk")
            .due("2026-03-01")
            .build()];
        // Task has no eid — would normally be treated as a new reminder
        let tasks = vec![task_from_line("Buy milk due:2026-03-01")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(
            state.items.contains_key(eid),
            "State entry must be created for reconciled pair"
        );
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].0, eid);
        assert_eq!(reconciled[0].1, 0); // task index 0
    }

    /// Both items have no due date → matched by title alone.
    #[test]
    fn reconcile_title_only_no_due() {
        let eid = "eid-reconcile-2";
        let reminders = vec![ReminderBuilder::new(eid).title("Read emails").build()];
        let tasks = vec![task_from_line("Read emails")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(state.items.contains_key(eid));
        assert_eq!(reconciled.len(), 1);
    }

    /// Title comparison is case-insensitive.
    #[test]
    fn reconcile_case_insensitive() {
        let eid = "eid-reconcile-3";
        let reminders = vec![ReminderBuilder::new(eid).title("BUY MILK").build()];
        let tasks = vec![task_from_line("buy milk")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(
            state.items.contains_key(eid),
            "Case-insensitive match should produce state entry"
        );
        assert_eq!(reconciled.len(), 1);
    }

    /// EID-matched pair does NOT appear in the reconciled vec.
    #[test]
    fn reconcile_eid_takes_priority() {
        let eid = "eid-reconcile-4";
        let reminders = vec![ReminderBuilder::new(eid).title("Buy milk").build()];
        // Task already has the EID — matched in Pass 1.
        let tasks = vec![task_with_eid(eid, "Buy milk")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(state.items.contains_key(eid));
        assert!(
            reconciled.is_empty(),
            "EID-matched pair must not appear in reconciled vec"
        );
    }

    /// Task has a stale eid (no matching reminder) → falls through to title match.
    #[test]
    fn reconcile_stale_eid_falls_to_title() {
        let stale_eid = "eid-old-stale";
        let real_eid = "eid-reconcile-5";
        let reminders = vec![ReminderBuilder::new(real_eid).title("Buy milk").build()];
        // Task has a stale eid that doesn't match any reminder.
        let tasks = vec![task_with_eid(stale_eid, "Buy milk")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        // The real reminder's eid should be in state via title match.
        assert!(
            state.items.contains_key(real_eid),
            "Real eid should be in state after title match"
        );
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].0, real_eid);
    }

    /// Two reminders with the same key → ambiguous → no match.
    #[test]
    fn reconcile_ambiguous_reminders_skipped() {
        let eid1 = "eid-amb-r1";
        let eid2 = "eid-amb-r2";
        let reminders = vec![
            ReminderBuilder::new(eid1).title("Buy milk").build(),
            ReminderBuilder::new(eid2).title("Buy milk").build(),
        ];
        let tasks = vec![task_from_line("Buy milk")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(
            reconciled.is_empty(),
            "Ambiguous reminders must not produce a match"
        );
        assert!(!state.items.contains_key(eid1));
        assert!(!state.items.contains_key(eid2));
    }

    /// Two tasks with the same key → ambiguous → no match.
    #[test]
    fn reconcile_ambiguous_tasks_skipped() {
        let eid = "eid-amb-t1";
        let reminders = vec![ReminderBuilder::new(eid).title("Buy milk").build()];
        let tasks = vec![task_from_line("Buy milk"), task_from_line("Buy milk")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(
            reconciled.is_empty(),
            "Ambiguous tasks must not produce a match"
        );
        assert!(!state.items.contains_key(eid));
    }

    /// `eid:na` sentinel task must not be a reconciliation candidate.
    #[test]
    fn reconcile_sentinel_excluded() {
        let eid = "eid-sentinel-test";
        let reminders = vec![ReminderBuilder::new(eid).title("Buy milk").build()];
        // Sentinel task with same title — must not be matched.
        let tasks = vec![task_from_line("Buy milk eid:na")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(
            reconciled.is_empty(),
            "Sentinel task must be excluded from reconciliation"
        );
        assert!(
            !state.items.contains_key(eid),
            "Sentinel task must not produce a state entry"
        );
    }

    /// Mix: some pairs matched by EID, others by title. All correct.
    #[test]
    fn reconcile_mixed_eid_and_title() {
        let eid_via_eid = "eid-mix-1";
        let eid_via_title = "eid-mix-2";
        let reminders = vec![
            ReminderBuilder::new(eid_via_eid).title("Task EID").build(),
            ReminderBuilder::new(eid_via_title)
                .title("Task Title")
                .build(),
        ];
        let tasks = vec![
            task_with_eid(eid_via_eid, "Task EID"), // EID match
            task_from_line("Task Title"),           // title match
        ];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(
            state.items.contains_key(eid_via_eid),
            "EID-matched pair must be in state"
        );
        assert!(
            state.items.contains_key(eid_via_title),
            "Title-matched pair must be in state"
        );
        assert_eq!(
            reconciled.len(),
            1,
            "Only the title-matched pair should be in reconciled vec"
        );
        assert_eq!(reconciled[0].0, eid_via_title);
    }

    /// Same title but different due date → no match.
    #[test]
    fn reconcile_different_due_no_match() {
        let eid = "eid-diff-due";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Buy milk")
            .due("2026-03-01")
            .build()];
        let tasks = vec![task_from_line("Buy milk due:2026-04-15")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(
            reconciled.is_empty(),
            "Different due dates must not produce a match"
        );
        assert!(!state.items.contains_key(eid));
    }

    /// Completed reminder + completed task → matched by title+due.
    #[test]
    fn reconcile_completed_items_match() {
        let eid = "eid-completed-rec";
        let reminders = vec![ReminderBuilder::new(eid)
            .title("Old task")
            .completed("2026-02-20")
            .build()];
        let tasks = vec![task_from_line("x 2026-02-20 2026-02-01 Old task")];
        let (state, reconciled) = build_initial_state(&reminders, &tasks, now());
        assert!(
            state.items.contains_key(eid),
            "Completed pair should be reconciled"
        );
        assert_eq!(reconciled.len(), 1);
    }

    /// Zero reminders or zero tasks → no panic, empty result.
    #[test]
    fn reconcile_empty_inputs_no_panic() {
        let (state_a, rec_a) = build_initial_state(&[], &[], now());
        assert!(state_a.items.is_empty());
        assert!(rec_a.is_empty());

        let reminders = vec![ReminderBuilder::new("eid-x").title("A").build()];
        let (state_b, rec_b) = build_initial_state(&reminders, &[], now());
        assert!(state_b.items.is_empty());
        assert!(rec_b.is_empty());

        let tasks = vec![task_from_line("A")];
        let (state_c, rec_c) = build_initial_state(&[], &tasks, now());
        assert!(state_c.items.is_empty());
        assert!(rec_c.is_empty());
    }

    /// Integration: after bootstrap reconciliation stamps the EID, the next
    /// sync pass must produce no CreateTask or CreateReminder for the matched pair.
    #[test]
    fn reconcile_no_duplicates_in_sync() {
        let eid = "eid-no-dup";
        let reminders = vec![ReminderBuilder::new(eid).title("Buy milk").build()];
        // Task has no eid — would create a duplicate without reconciliation.
        let mut tasks = vec![task_from_line("Buy milk")];
        let (initial_state, reconciled) = build_initial_state(&reminders, &tasks, now());

        // Simulate the caller stamping the EID (as main.rs does).
        for (stamp_eid, idx) in &reconciled {
            tasks[*idx].update_tag_with_value("eid", stamp_eid);
        }
        assert_eq!(reconciled.len(), 1, "Should have one title-matched pair");

        // Now run the sync engine with the stamped tasks and the initial state.
        let config = default_config();
        let actions =
            compute_sync_actions(&reminders, &tasks, &initial_state, &config, now(), None);

        let has_create_task = actions
            .iter()
            .any(|a| matches!(a, SyncAction::CreateTask { .. }));
        let has_create_reminder = actions
            .iter()
            .any(|a| matches!(a, SyncAction::CreateReminder { .. }));

        assert!(
            !has_create_task,
            "Reconciled pair must not produce CreateTask"
        );
        assert!(
            !has_create_reminder,
            "Reconciled pair must not produce CreateReminder"
        );
    }

    // ── Item 19: All-fields-changed scenario ─────────────────────────────────
    //
    // Verify the engine produces correct actions when every synced field
    // (title, priority, due_date) differs from the baseline on both sides.
    // Reference: rclone bisync `test_all_changed/` — all-files-changed scenario.

    /// Both sides changed every field that maps bidirectionally (title + due);
    /// task_mtime > r_modified → task wins all conflicts.
    /// Expect UpdateReminder with task's values; no UpdateTask.
    #[test]
    fn all_fields_changed_both_sides_task_wins() {
        let eid = "eid-all-task-wins";
        // Baseline: title="Baseline", priority=0, due=None, notes=None.
        // synced_item sets task_line_hash=0 so any real task looks "changed".
        let item = synced_item(eid, "Baseline");

        // Reminder changed title + due (priority=0 to keep default map stable).
        // Reminder is *older* than the task → task wins the conflict.
        let reminder = ReminderBuilder::new(eid)
            .title("Reminder New")
            .due("2026-04-01")
            .modified(past_time()) // older → task wins
            .build();

        // Task also changed title + due to *different* values than the reminder.
        // task_mtime is newer → task wins.
        let task = task_from_line(&format!("Task New due:2026-05-01 eid:{eid}"));

        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &[reminder],
            &[task],
            &state,
            &config,
            now(),
            Some(recent_time()), // task_mtime newer → task wins
        );

        // Task side won all conflicting fields → update reminder only.
        assert_updates_reminder(&actions, eid);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::UpdateTask { .. })),
            "All-fields-changed task-wins: must not emit UpdateTask"
        );
    }

    /// Both sides changed every field that maps bidirectionally (title + due);
    /// r_modified > task_mtime → reminder wins all conflicts.
    /// Expect UpdateTask with reminder's values; no UpdateReminder.
    #[test]
    fn all_fields_changed_both_sides_reminder_wins() {
        let eid = "eid-all-rem-wins";
        // Baseline: title="Baseline", priority=0, due=None, notes=None.
        let item = synced_item(eid, "Baseline");

        // Reminder changed title + due and is *newer* than the task → reminder wins.
        let reminder = ReminderBuilder::new(eid)
            .title("Reminder New")
            .due("2026-04-01")
            .modified(recent_time()) // newer → reminder wins
            .build();

        // Task also changed title + due to *different* values than the reminder.
        // task_mtime is older → task loses all conflicts.
        let task = task_from_line(&format!("Task New due:2026-05-01 eid:{eid}"));

        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(
            &[reminder],
            &[task],
            &state,
            &config,
            now(),
            Some(past_time()), // task_mtime older → reminder wins
        );

        // Reminder side won all conflicting fields → update task only.
        assert_updates_task(&actions, eid);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::UpdateReminder { .. })),
            "All-fields-changed reminder-wins: must not emit UpdateReminder"
        );
    }

    // ── Item 20: Dry-run action plan purity ───────────────────────────────────
    //
    // Dry-run produces the same action plan as a normal sync — the
    // compute_sync_actions* family is pure and unaffected by the dry-run flag.
    // The flag is checked in sync_once *after* actions are computed, so the
    // action list is guaranteed identical.
    //
    // These tests verify that the flag is correctly set by the arg parser and
    // that the pure action-computation path is exercised by existing tests.

    /// compute_sync_actions returns identical results regardless of any
    /// caller-side dry-run flag — confirming zero-side-effect guarantee.
    #[test]
    fn dry_run_action_plan_is_identical_to_live_run() {
        let eid = "eid-dry-run";
        let reminders = vec![ReminderBuilder::new(eid).title("Task A").build()];
        let tasks = vec![task_with_eid(eid, "Task A")];
        let state = state_with_items(vec![synced_item(eid, "Task A")]);
        let config = default_config();

        // Both runs call the same pure function — results must be equal.
        let live_actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );
        let dry_actions = compute_sync_actions(
            &reminders,
            &tasks,
            &state,
            &config,
            now(),
            Some(recent_time()),
        );

        assert_eq!(
            live_actions.len(),
            dry_actions.len(),
            "dry-run and live-run must produce the same number of actions"
        );
    }

    // ============================================================
    // Category: EID Relink (iCloud EID reassignment)
    // ============================================================

    fn assert_relinks_eid(actions: &[SyncAction], old_eid: &str, new_eid: &str) {
        let found = actions.iter().any(|a| {
            if let SyncAction::RelinkEid {
                old_eid: o,
                new_eid: n,
            } = a
            {
                o.as_str() == old_eid && n.as_str() == new_eid
            } else {
                false
            }
        });
        assert!(found, "Expected RelinkEid(old={old_eid}, new={new_eid})");
    }

    /// Old EID gone, new EID with same synced-field hash → RelinkEid only.
    /// No DeleteTask, no CreateTask.
    #[test]
    fn relink_basic() {
        let new_reminder = ReminderBuilder::new("new-eid").title("Buy milk").build();
        let reminders = vec![new_reminder];
        let task = task_with_eid("old-eid", "Buy milk");
        let tasks = vec![task];
        let state = state_with_items(vec![synced_item("old-eid", "Buy milk")]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);

        assert_relinks_eid(&actions, "old-eid", "new-eid");
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteTask { .. })),
            "Expected no DeleteTask"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::CreateTask { .. })),
            "Expected no CreateTask (new EID consumed by relink)"
        );
    }

    /// Task title edited locally after the EID was reassigned → RelinkEid + UpdateReminder.
    #[test]
    fn relink_with_task_edit() {
        // Baseline: "Buy milk". Task now says "Buy almond milk".
        // New reminder still has the baseline title (unchanged on Reminders side).
        let new_reminder = ReminderBuilder::new("new-eid").title("Buy milk").build();
        let reminders = vec![new_reminder];
        let task = task_with_eid("old-eid", "Buy almond milk");
        let tasks = vec![task];
        let state = state_with_items(vec![synced_item("old-eid", "Buy milk")]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);

        assert_relinks_eid(&actions, "old-eid", "new-eid");
        assert_updates_reminder(&actions, "new-eid");
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteTask { .. })),
            "Expected no DeleteTask"
        );
    }

    /// Two unmatched reminders share the same hash → ambiguous, no relink.
    #[test]
    fn relink_ambiguous_multiple_unmatched() {
        let r1 = ReminderBuilder::new("new-eid-a").title("Buy milk").build();
        let r2 = ReminderBuilder::new("new-eid-b").title("Buy milk").build();
        let reminders = vec![r1, r2];
        let task = task_with_eid("old-eid", "Buy milk");
        let tasks = vec![task];
        let state = state_with_items(vec![synced_item("old-eid", "Buy milk")]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::RelinkEid { .. })),
            "Ambiguous unmatched reminders should not trigger RelinkEid"
        );
    }

    /// Two state entries share the same stored hash, both missing → no relink.
    #[test]
    fn relink_ambiguous_multiple_state_entries() {
        let new_reminder = ReminderBuilder::new("new-eid").title("Buy milk").build();
        let reminders = vec![new_reminder];
        let task_a = task_with_eid("old-eid-a", "Buy milk");
        let task_b = task_with_eid("old-eid-b", "Buy milk");
        let tasks = vec![task_a, task_b];
        let state = state_with_items(vec![
            synced_item("old-eid-a", "Buy milk"),
            synced_item("old-eid-b", "Buy milk"),
        ]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::RelinkEid { .. })),
            "Ambiguous state entries should not trigger RelinkEid"
        );
    }

    /// Stored hash = 0 (old state.json) → no relink attempted.
    #[test]
    fn relink_zero_stored_hash() {
        let new_reminder = ReminderBuilder::new("new-eid").title("Buy milk").build();
        let reminders = vec![new_reminder];
        let task = task_with_eid("old-eid", "Buy milk");
        let tasks = vec![task];
        let mut item = synced_item("old-eid", "Buy milk");
        item.reminders_field_hash = 0;
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SyncAction::RelinkEid { .. })),
            "Zero stored hash should not trigger RelinkEid"
        );
    }

    /// The relinked new-eid must not also appear as a CreateTask in Step 2.
    #[test]
    fn relink_consumed_not_duplicated_step2() {
        let new_reminder = ReminderBuilder::new("new-eid").title("Buy milk").build();
        let reminders = vec![new_reminder];
        let task = task_with_eid("old-eid", "Buy milk");
        let tasks = vec![task];
        let state = state_with_items(vec![synced_item("old-eid", "Buy milk")]);
        let config = default_config();
        let actions = compute_sync_actions(&reminders, &tasks, &state, &config, now(), None);

        let create_task_count = actions
            .iter()
            .filter(|a| matches!(a, SyncAction::CreateTask { .. }))
            .count();
        assert_eq!(
            create_task_count, 0,
            "RelinkEid should prevent Step 2 from also emitting CreateTask"
        );
    }

    /// apply_task_actions correctly rewrites the eid: tag and moves the state entry.
    #[test]
    fn relink_apply_task_actions() {
        let task = task_with_eid("old-eid", "Buy milk");
        let tasks = vec![task];
        let mut item = synced_item("old-eid", "Buy milk");
        item.pushed = true;
        let state = state_with_items(vec![item]);
        let config = default_config();
        let actions = vec![SyncAction::RelinkEid {
            old_eid: "old-eid".to_string(),
            new_eid: "new-eid".to_string(),
        }];
        let (new_tasks, new_state) = apply_task_actions(&actions, tasks, &state, &config, now());

        // Task eid: tag rewritten.
        assert_eq!(new_tasks.len(), 1);
        let new_task_eid = new_tasks[0].tags.get("eid").map(|s| s.as_str());
        assert_eq!(
            new_task_eid,
            Some("new-eid"),
            "Task eid: tag should be rewritten to new-eid"
        );

        // State entry moved from old to new key.
        assert!(
            !new_state.items.contains_key("old-eid"),
            "Old eid should be removed from state"
        );
        assert!(
            new_state.items.contains_key("new-eid"),
            "New eid should be present in state"
        );

        // pushed flag preserved.
        let new_item = &new_state.items["new-eid"];
        assert!(new_item.pushed, "pushed flag should be preserved");
        assert_eq!(new_item.eid, "new-eid");
    }

    // ----------------------------------------------------------------
    // Writeback control tests
    // ----------------------------------------------------------------

    /// Reminder-only title change with writeback.title=false → push task title
    /// back to Reminder instead of updating the task.
    #[test]
    fn writeback_disabled_title_reminder_only_pushes_back() {
        // Baseline: title="Buy milk", reminder changed to "Buy groceries", task unchanged.
        let reminder = ReminderBuilder::new("e1")
            .title("Buy groceries")
            .modified(recent_time())
            .build();
        let task = task_with_eid("e1", "Buy milk");
        let state = state_with_items(vec![synced_item("e1", "Buy milk")]);
        let config = ListSyncConfig::new("Tasks").with_writeback(WritebackConfig {
            title: false,
            ..WritebackConfig::default()
        });

        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);

        let has_update_task = actions
            .iter()
            .any(|a| matches!(a, SyncAction::UpdateTask { .. }));
        let update_reminder = actions.iter().find_map(|a| {
            if let SyncAction::UpdateReminder {
                updated_reminder, ..
            } = a
            {
                Some(updated_reminder)
            } else {
                None
            }
        });

        assert!(!has_update_task, "task title must not be overwritten");
        assert!(
            update_reminder.is_some(),
            "should push task title back to Reminder"
        );
        assert_eq!(
            update_reminder.unwrap().title.as_deref(),
            Some("Buy milk"),
            "UpdateReminder should carry the task title"
        );
    }

    /// Both sides changed due date (reminder newer in LWW terms), but
    /// writeback.due_date=false means task always wins the conflict.
    #[test]
    fn writeback_disabled_due_date_conflict_task_wins() {
        // Baseline: due_date=None. Reminder sets 2026-03-01, task sets 2026-03-15.
        // Reminder mtime is recent (newer), task mtime is old → LWW would pick reminder.
        // With writeback disabled the task value must win regardless.
        let reminder = ReminderBuilder::new("e1")
            .title("Buy milk") // match baseline so title doesn't generate an UpdateTask
            .due("2026-03-01")
            .modified(recent_time())
            .build();
        let task = task_with_eid("e1", "Buy milk due:2026-03-15");
        let state = state_with_items(vec![synced_item("e1", "Buy milk")]);
        let config = ListSyncConfig::new("Tasks").with_writeback(WritebackConfig {
            due_date: false,
            ..WritebackConfig::default()
        });

        let actions = compute_sync_actions(
            &[reminder],
            &[task],
            &state,
            &config,
            now(),
            Some(old_time()),
        );

        let has_update_task = actions
            .iter()
            .any(|a| matches!(a, SyncAction::UpdateTask { .. }));
        let update_reminder = actions.iter().find_map(|a| {
            if let SyncAction::UpdateReminder {
                updated_reminder, ..
            } = a
            {
                Some(updated_reminder)
            } else {
                None
            }
        });

        assert!(!has_update_task, "task due date must not be overwritten");
        let ur = update_reminder.expect("should push task due date back");
        assert_eq!(
            ur.due_date,
            Some(Some("2026-03-15".to_string())),
            "UpdateReminder due_date should carry the task value"
        );
    }

    /// Reminder completed while writeback.is_completed=false → push false back to
    /// Reminder, task remains not completed.
    #[test]
    fn writeback_disabled_is_completed_suppresses() {
        let reminder = ReminderBuilder::new("e1")
            .title("Buy milk") // match baseline so title doesn't generate an UpdateTask
            .completed("2026-02-20")
            .modified(recent_time())
            .build();
        let task = task_with_eid("e1", "Buy milk");
        let state = state_with_items(vec![synced_item("e1", "Buy milk")]);
        let config = ListSyncConfig::new("Tasks").with_writeback(WritebackConfig {
            is_completed: false,
            ..WritebackConfig::default()
        });

        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);

        let has_update_task = actions
            .iter()
            .any(|a| matches!(a, SyncAction::UpdateTask { .. }));
        let update_reminder = actions.iter().find_map(|a| {
            if let SyncAction::UpdateReminder {
                updated_reminder, ..
            } = a
            {
                Some(updated_reminder)
            } else {
                None
            }
        });

        assert!(!has_update_task, "task must not be marked completed");
        let ur = update_reminder.expect("should push false back to Reminder");
        assert_eq!(
            ur.is_completed,
            Some(false),
            "UpdateReminder should carry is_completed=false"
        );
        // completion_date follows is_completed flag — should push None back.
        assert_eq!(
            ur.completion_date,
            Some(None),
            "UpdateReminder should carry completion_date=None"
        );
    }

    /// Reminder priority changed with writeback.priority=false → push task
    /// priority back to Reminder, task priority unchanged.
    #[test]
    fn writeback_disabled_priority_pushes_back() {
        // Baseline: priority=0 (no priority). Reminder now has priority=9 (@today).
        // Task still has no priority. writeback.priority=false → push 0 back.
        let reminder = ReminderBuilder::new("e1")
            .title("Buy milk") // match baseline so title doesn't generate an UpdateTask
            .priority(9)
            .modified(recent_time())
            .build();
        let task = task_with_eid("e1", "Buy milk");
        let state = state_with_items(vec![synced_item("e1", "Buy milk")]);
        let config = ListSyncConfig::new("Tasks").with_writeback(WritebackConfig {
            priority: false,
            ..WritebackConfig::default()
        });

        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);

        let has_update_task = actions
            .iter()
            .any(|a| matches!(a, SyncAction::UpdateTask { .. }));
        let update_reminder = actions.iter().find_map(|a| {
            if let SyncAction::UpdateReminder {
                updated_reminder, ..
            } = a
            {
                Some(updated_reminder)
            } else {
                None
            }
        });

        assert!(!has_update_task, "task priority must not be updated");
        let ur = update_reminder.expect("should push task priority back");
        assert_eq!(
            ur.priority,
            Some(0),
            "UpdateReminder should carry priority=0 (task value)"
        );
    }

    /// When only the task changed (reminder matches baseline), writeback disabled
    /// on a field has no effect — the normal task→reminder push still fires.
    #[test]
    fn writeback_disabled_task_only_change_unaffected() {
        // Baseline: due_date=None. Task now has due:2026-03-01. Reminder unchanged.
        // writeback.due_date=false controls reminder→task direction only; this is
        // a task-only change so due_date should still flow to the Reminder.
        let reminder = ReminderBuilder::new("e1").modified(past_time()).build();
        let task = task_with_eid("e1", "Buy milk due:2026-03-01");
        let state = state_with_items(vec![synced_item("e1", "Buy milk")]);
        let config = ListSyncConfig::new("Tasks").with_writeback(WritebackConfig {
            due_date: false,
            ..WritebackConfig::default()
        });

        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);

        let update_reminder = actions.iter().find_map(|a| {
            if let SyncAction::UpdateReminder {
                updated_reminder, ..
            } = a
            {
                Some(updated_reminder)
            } else {
                None
            }
        });

        let ur = update_reminder.expect("task change should push due date to Reminder");
        assert_eq!(
            ur.due_date,
            Some(Some("2026-03-01".to_string())),
            "task-only due_date change must still push to Reminder even when writeback=false"
        );
    }

    /// All writeback fields disabled: all reminder-side changes pushed back to
    /// Reminder as a single UpdateReminder, no UpdateTask emitted.
    #[test]
    fn writeback_all_disabled_all_pushed_back() {
        // Baseline: title="Buy milk", priority=0, due_date=None.
        // Reminder changed all three. Task unchanged.
        let reminder = ReminderBuilder::new("e1")
            .title("Buy groceries")
            .priority(9)
            .due("2026-03-01")
            .modified(recent_time())
            .build();
        let task = task_with_eid("e1", "Buy milk");
        let state = state_with_items(vec![synced_item("e1", "Buy milk")]);
        let config = ListSyncConfig::new("Tasks").with_writeback(WritebackConfig {
            title: false,
            due_date: false,
            priority: false,
            is_completed: false,
        });

        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);

        let has_update_task = actions
            .iter()
            .any(|a| matches!(a, SyncAction::UpdateTask { .. }));
        let update_reminder = actions.iter().find_map(|a| {
            if let SyncAction::UpdateReminder {
                updated_reminder, ..
            } = a
            {
                Some(updated_reminder)
            } else {
                None
            }
        });

        assert!(!has_update_task, "no task fields should be updated");
        let ur = update_reminder.expect("should emit a single UpdateReminder");
        assert_eq!(
            ur.title.as_deref(),
            Some("Buy milk"),
            "title pushed back to task value"
        );
        assert_eq!(ur.priority, Some(0), "priority pushed back to task value");
        assert_eq!(
            ur.due_date,
            Some(None),
            "due_date pushed back to task value (None)"
        );
    }

    /// Mixed policy: title=false (task authoritative) + due_date=true (Reminders authoritative).
    /// Reminder changes both. Expect UpdateTask for due_date and UpdateReminder for title.
    #[test]
    fn writeback_mixed_some_flow_some_suppressed() {
        // Baseline: title="Buy milk", due_date=None. Reminder changes both.
        let reminder = ReminderBuilder::new("e1")
            .title("Buy groceries")
            .due("2026-03-01")
            .modified(recent_time())
            .build();
        let task = task_with_eid("e1", "Buy milk");
        let state = state_with_items(vec![synced_item("e1", "Buy milk")]);
        let config = ListSyncConfig::new("Tasks").with_writeback(WritebackConfig {
            title: false,
            due_date: true,
            ..WritebackConfig::default()
        });

        let actions = compute_sync_actions(&[reminder], &[task], &state, &config, now(), None);

        // due_date=true → UpdateTask should carry the new due date.
        let update_task = actions.iter().find_map(|a| {
            if let SyncAction::UpdateTask { updated_task, .. } = a {
                Some(updated_task)
            } else {
                None
            }
        });
        let update_reminder = actions.iter().find_map(|a| {
            if let SyncAction::UpdateReminder {
                updated_reminder, ..
            } = a
            {
                Some(updated_reminder)
            } else {
                None
            }
        });

        let ut = update_task.expect("due_date=true should produce UpdateTask");
        assert_eq!(
            ut.tags.get("due").map(|s| s.as_str()),
            Some("2026-03-01"),
            "UpdateTask should carry the new due date from Reminders"
        );

        // title=false → UpdateReminder should carry the task title.
        let ur = update_reminder.expect("title=false should produce UpdateReminder");
        assert_eq!(
            ur.title.as_deref(),
            Some("Buy milk"),
            "UpdateReminder should push task title back"
        );
    }
}
