use log::info;
use todo_lib::todotxt::{CompletionConfig, CompletionDateMode, CompletionMode, Task};

use crate::sync::actions::SyncAction;

/// For each action that marks a recurring task as completed, call `todo::done()` on a temp
/// copy of the pre-completion task to obtain the next recurring instance.
///
/// Returns a vec of newly-spawned tasks (no `eid:` tag) to be appended to the task list and
/// processed in a follow-up sync pass.
///
/// # Split design
///
/// The sync engine (`compute_sync_actions`) is a pure function that cannot call `todo::done()`
/// because that requires `&mut TaskVec`.  Recurrence spawning is therefore handled here at the
/// orchestration layer, using the pre-completion task list that is still available before
/// `apply_task_actions` is called.
pub fn collect_recurrence_spawns(actions: &[SyncAction], current_tasks: &[Task]) -> Vec<Task> {
    let mut spawns = Vec::new();

    for action in actions {
        let (eid, updated_task) = match action {
            SyncAction::UpdateTask { eid, updated_task } => (eid, updated_task),
            SyncAction::MergeConflict {
                eid, updated_task, ..
            } => (eid, updated_task),
            _ => continue,
        };

        // Only care about tasks newly completed that carry a recurrence rule.
        if !updated_task.finished || updated_task.recurrence.is_none() {
            continue;
        }

        // Find the old (pre-completion) task by eid. Skip if already finished
        // (avoids double-spawning if the task was completed on a previous pass).
        let old_task = match current_tasks
            .iter()
            .find(|t| t.tags.get("eid").map(|e| e.as_str()) == Some(eid.as_str()))
        {
            Some(t) if !t.finished => t,
            _ => continue,
        };

        // Use todo::done() as the canonical recurrence function. We pass it the
        // pre-completion task; it marks temp[0] done and, if recurrence conditions
        // are met (rec_until guard), appends temp[1] (the next recurring instance).
        let mut temp = vec![old_task.clone()];
        todo_lib::todo::done(
            &mut temp,
            None,
            CompletionConfig {
                completion_mode: CompletionMode::JustMark,
                completion_date_mode: CompletionDateMode::AlwaysSet,
            },
        );

        // Collect the new instance if one was actually spawned (rec_until guard
        // inside todo::done() may suppress it).
        if temp.len() > 1 {
            let mut spawn = temp.remove(1);
            // Strip inherited eid so the spawn is treated as a brand-new task on the
            // follow-up sync pass and gets a fresh eid from Reminders. Without this,
            // the done parent and the spawn would share the same eid, triggering the
            // duplicate-eid check in verify_post_sync.
            spawn.update_tag("eid:");
            info!(
                "Recurrence: spawning next instance of '{}' ({})",
                spawn.subject,
                spawn
                    .due_date
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "no due date".to_string())
            );
            spawns.push(spawn);
        }
    }

    spawns
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use todo_lib::todotxt::Task;

    use super::collect_recurrence_spawns;
    use crate::sync::actions::SyncAction;
    use crate::sync::engine::verify_post_sync;
    use crate::sync::state::SyncState;

    fn base_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 2, 25).unwrap()
    }

    fn task(line: &str) -> Task {
        Task::parse(line, base_date())
    }

    /// Build a completed updated_task for use in UpdateTask actions.
    fn completed_task(line: &str) -> Task {
        // Prepend completion marker so todo_lib sets finished=true.
        task(&format!("x 2026-02-25 2026-01-01 {line}"))
    }

    // ----------------------------------------------------------------
    // Recurring completion → spawn
    // ----------------------------------------------------------------

    #[test]
    fn recurring_completion_spawns_next_instance() {
        // rec:+1w (strict mode) advances from the original due date, making this deterministic.
        let old = task("Buy milk due:2026-02-25 rec:+1w eid:eid1");
        let updated = completed_task("Buy milk due:2026-02-25 rec:+1w eid:eid1");

        let actions = vec![SyncAction::UpdateTask {
            eid: "eid1".to_string(),
            updated_task: updated,
        }];

        let spawns = collect_recurrence_spawns(&actions, &[old]);
        assert_eq!(spawns.len(), 1, "expected exactly one spawn");
        assert_eq!(
            spawns[0].due_date,
            NaiveDate::from_ymd_opt(2026, 3, 4),
            "next due date should be 1 week after original due"
        );
        assert!(
            spawns[0].tags.get("eid").is_none(),
            "spawned task must not inherit parent eid"
        );
    }

    // ----------------------------------------------------------------
    // until: tag suppresses spawn
    // ----------------------------------------------------------------

    #[test]
    fn until_tag_suppresses_spawn() {
        // rec:+1w (strict): next due = 2026-03-04. until:2026-03-01 < 2026-03-04 → suppressed.
        let old = task("Buy milk due:2026-02-25 rec:+1w until:2026-03-01 eid:eid1");
        let updated = completed_task("Buy milk due:2026-02-25 rec:+1w until:2026-03-01 eid:eid1");

        let actions = vec![SyncAction::UpdateTask {
            eid: "eid1".to_string(),
            updated_task: updated,
        }];

        let spawns = collect_recurrence_spawns(&actions, &[old]);
        assert_eq!(spawns.len(), 0, "spawn should be suppressed by until: tag");
    }

    // ----------------------------------------------------------------
    // Non-recurring task produces no spawn
    // ----------------------------------------------------------------

    #[test]
    fn non_recurring_completion_produces_no_spawn() {
        let old = task("Buy milk due:2026-02-25 eid:eid1");
        let updated = completed_task("Buy milk due:2026-02-25 eid:eid1");

        let actions = vec![SyncAction::UpdateTask {
            eid: "eid1".to_string(),
            updated_task: updated,
        }];

        let spawns = collect_recurrence_spawns(&actions, &[old]);
        assert_eq!(
            spawns.len(),
            0,
            "non-recurring task should produce no spawn"
        );
    }

    // ----------------------------------------------------------------
    // Already-completed old task → no spawn (double-spawn guard)
    // ----------------------------------------------------------------

    #[test]
    fn already_completed_old_task_produces_no_spawn() {
        // The old task is already finished — guard skips it to prevent double-spawning.
        let old = task("x 2026-02-20 2026-01-01 Buy milk due:2026-02-20 rec:1w eid:eid1");
        let updated = completed_task("Buy milk due:2026-02-25 rec:1w eid:eid1");

        let actions = vec![SyncAction::UpdateTask {
            eid: "eid1".to_string(),
            updated_task: updated,
        }];

        let spawns = collect_recurrence_spawns(&actions, &[old]);
        assert_eq!(
            spawns.len(),
            0,
            "already-completed old task should not produce a second spawn"
        );
    }

    // ----------------------------------------------------------------
    // Regression: spawn must not inherit parent eid (duplicate-eid bug)
    // ----------------------------------------------------------------

    /// Before the fix, `cleanup_cloned_task()` (in todo_lib) did not strip
    /// `eid:`, so the spawned task silently inherited the parent's eid.  After
    /// `apply_task_actions` marked the parent done, both the completed parent
    /// and the spawn had the same `eid:`, which `verify_post_sync` reported as
    /// a duplicate-eid issue on every subsequent sync cycle.
    #[test]
    fn spawn_eid_stripped_no_duplicate_eid_in_post_sync_task_list() {
        let eid = "eid-recurring";
        let old = task(&format!("Buy milk due:2026-02-25 rec:+1w eid:{eid}"));
        let updated = completed_task(&format!("Buy milk due:2026-02-25 rec:+1w eid:{eid}"));

        let actions = vec![SyncAction::UpdateTask {
            eid: eid.to_string(),
            updated_task: updated,
        }];

        let spawns = collect_recurrence_spawns(&actions, &[old.clone()]);
        assert_eq!(spawns.len(), 1);

        let spawn = &spawns[0];
        assert!(
            spawn.tags.get("eid").is_none(),
            "spawn must not carry the parent eid; got {:?}",
            spawn.tags.get("eid")
        );

        // Compose the post-sync task list as main.rs does: the completed
        // parent (still carrying its eid) plus the freshly spawned task.
        let completed_parent = task(&format!(
            "x 2026-02-25 2026-02-25 Buy milk due:2026-02-25 rec:+1w eid:{eid}"
        ));
        let post_sync_tasks = [completed_parent, spawn.clone()];

        // verify_post_sync should not report any duplicate-eid issue.
        let issues = verify_post_sync(&post_sync_tasks, &SyncState::default());
        assert!(
            issues.iter().all(|s| !s.contains("duplicate eid")),
            "unexpected duplicate-eid issue: {issues:?}"
        );
    }
}
