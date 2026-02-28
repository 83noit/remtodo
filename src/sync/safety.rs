use std::path::Path;

use crate::error::SyncError;
use crate::sync::actions::SyncAction;

/// Returns the bulk-deletion threshold for a list with `tracked` items.
///
/// `max_percent` is the configured ceiling (0–100); the function additionally
/// enforces a floor of 3 so the guard does not fire for tiny lists where a
/// single legitimate delete would exceed a strict percentage.
pub fn bulk_delete_threshold(tracked: usize, max_percent: u8) -> usize {
    let pct = max_percent.min(100) as usize;
    (tracked * pct / 100).max(3)
}

/// Safety guard: on the very first sync both sides can only grow.
///
/// Any deletion action here means state is corrupt or there is a bug —
/// abort before touching either file.
pub fn check_first_sync_no_deletions(
    actions: &[SyncAction],
    list_name: &str,
    state_path: &Path,
) -> Result<(), SyncError> {
    let deletion_count = actions
        .iter()
        .filter(|a| {
            matches!(
                a,
                SyncAction::DeleteTask { .. } | SyncAction::DeleteReminder { .. }
            )
        })
        .count();
    if deletion_count > 0 {
        return Err(SyncError::SafetyAbort(format!(
            "{deletion_count} deletion action(s) computed on first sync for list '{list_name}'. \
             This should never happen — delete {} to force a clean first sync.",
            state_path.display(),
        )));
    }
    Ok(())
}

/// Safety guard: bulk reminder deletion.
///
/// Deleting more than `max_delete_percent`% of the tracked reminders for a
/// list in a single sync is almost certainly a bug or data corruption, not
/// intentional user action. Configured via `max_delete_percent` in config.toml
/// (default 50). Set to 100 to disable.
pub fn check_bulk_deletion(
    actions: &[SyncAction],
    tracked_for_list: usize,
    list_name: &str,
    state_path: &Path,
    max_delete_percent: u8,
) -> Result<(), SyncError> {
    let reminder_deletes = actions
        .iter()
        .filter(|a| matches!(a, SyncAction::DeleteReminder { .. }))
        .count();
    let threshold = bulk_delete_threshold(tracked_for_list, max_delete_percent);
    if tracked_for_list > 0 && reminder_deletes > threshold {
        return Err(SyncError::SafetyAbort(format!(
            "Sync would delete {reminder_deletes}/{tracked_for_list} tracked reminders \
             for list '{list_name}' — exceeds safety threshold of {threshold} \
             ({max_delete_percent}%). \
             If intentional, raise max_delete_percent in config.toml or delete {} to reset state.",
            state_path.display(),
        )));
    }
    Ok(())
}

/// Safety guard: task count coherence.
///
/// After applying actions, the surviving task count plus the explicitly deleted
/// tasks must equal or exceed the pre-sync count. A shortfall means tasks were
/// silently dropped.
pub fn check_task_count_coherence(
    pre_count: usize,
    post_count: usize,
    delete_count: usize,
    list_name: &str,
) -> Result<(), SyncError> {
    if post_count + delete_count < pre_count {
        return Err(SyncError::SafetyAbort(format!(
            "Task count fell from {pre_count} to {post_count} after sync for list '{list_name}' \
             (only {delete_count} explicit deletion(s) account for the difference). \
             Aborting to prevent data loss.",
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::sync::actions::ReminderUpdate;

    fn dummy_state_path() -> PathBuf {
        PathBuf::from("/tmp/state.json")
    }

    fn dummy_reminder_update(eid: &str) -> ReminderUpdate {
        ReminderUpdate {
            eid: eid.to_string(),
            list_name: "Tasks".to_string(),
            title: None,
            priority: None,
            is_completed: None,
            completion_date: None,
            due_date: None,
            notes: None,
        }
    }

    // ── check_first_sync_no_deletions ─────────────────────────────────────────

    #[test]
    fn first_sync_empty_actions_passes() {
        let actions: Vec<SyncAction> = vec![];
        assert!(check_first_sync_no_deletions(&actions, "Tasks", &dummy_state_path()).is_ok());
    }

    #[test]
    fn first_sync_no_deletions_passes() {
        let actions = vec![SyncAction::UpdateReminder {
            eid: "eid-1".to_string(),
            updated_reminder: dummy_reminder_update("eid-1"),
        }];
        assert!(check_first_sync_no_deletions(&actions, "Tasks", &dummy_state_path()).is_ok());
    }

    #[test]
    fn first_sync_with_delete_task_aborts() {
        let actions = vec![SyncAction::DeleteTask {
            eid: "eid-1".to_string(),
        }];
        let result = check_first_sync_no_deletions(&actions, "Tasks", &dummy_state_path());
        assert!(matches!(result, Err(SyncError::SafetyAbort(_))));
    }

    #[test]
    fn first_sync_with_delete_reminder_aborts() {
        let actions = vec![SyncAction::DeleteReminder {
            eid: "eid-2".to_string(),
        }];
        let result = check_first_sync_no_deletions(&actions, "Tasks", &dummy_state_path());
        assert!(matches!(result, Err(SyncError::SafetyAbort(_))));
    }

    // ── check_bulk_deletion ───────────────────────────────────────────────────

    #[test]
    fn bulk_delete_empty_tracked_passes() {
        // 0 tracked → guard skipped regardless of delete count
        let actions = vec![
            SyncAction::DeleteReminder {
                eid: "eid-1".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-2".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-3".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-4".to_string(),
            },
        ];
        assert!(check_bulk_deletion(&actions, 0, "Tasks", &dummy_state_path(), 50).is_ok());
    }

    #[test]
    fn bulk_delete_at_threshold_passes() {
        // tracked=6, threshold=max(6*50/100,3)=3, deletes=3, 3>3 is false → passes
        let actions = vec![
            SyncAction::DeleteReminder {
                eid: "eid-1".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-2".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-3".to_string(),
            },
        ];
        assert!(check_bulk_deletion(&actions, 6, "Tasks", &dummy_state_path(), 50).is_ok());
    }

    #[test]
    fn bulk_delete_over_threshold_aborts() {
        // tracked=6, threshold=3, deletes=4, 4>3 → SafetyAbort
        let actions = vec![
            SyncAction::DeleteReminder {
                eid: "eid-1".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-2".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-3".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-4".to_string(),
            },
        ];
        let result = check_bulk_deletion(&actions, 6, "Tasks", &dummy_state_path(), 50);
        assert!(matches!(result, Err(SyncError::SafetyAbort(_))));
    }

    #[test]
    fn bulk_delete_floor_of_three() {
        // tracked=4, threshold=max(4*50/100,3)=max(2,3)=3, deletes=3, 3>3 false → passes
        let actions = vec![
            SyncAction::DeleteReminder {
                eid: "eid-1".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-2".to_string(),
            },
            SyncAction::DeleteReminder {
                eid: "eid-3".to_string(),
            },
        ];
        assert!(check_bulk_deletion(&actions, 4, "Tasks", &dummy_state_path(), 50).is_ok());
    }

    #[test]
    fn bulk_delete_threshold_arithmetic() {
        // Default 50%
        assert_eq!(bulk_delete_threshold(0, 50), 3);
        assert_eq!(bulk_delete_threshold(1, 50), 3);
        assert_eq!(bulk_delete_threshold(5, 50), 3);
        assert_eq!(bulk_delete_threshold(6, 50), 3);
        assert_eq!(bulk_delete_threshold(7, 50), 3);
        assert_eq!(bulk_delete_threshold(8, 50), 4);
        assert_eq!(bulk_delete_threshold(10, 50), 5);
        assert_eq!(bulk_delete_threshold(100, 50), 50);
        // Custom percentages
        assert_eq!(bulk_delete_threshold(10, 25), 3); // 10*25/100=2, floor→3
        assert_eq!(bulk_delete_threshold(20, 25), 5); // 20*25/100=5
        assert_eq!(bulk_delete_threshold(10, 80), 8); // 10*80/100=8
        assert_eq!(bulk_delete_threshold(10, 100), 10); // 100% = disabled effectively
    }

    #[test]
    fn bulk_delete_custom_percent_lower_threshold_aborts_earlier() {
        // With 25%, tracked=20 → threshold=max(5,3)=5. 6 deletes > 5 → SafetyAbort.
        let actions: Vec<SyncAction> = (1..=6)
            .map(|i| SyncAction::DeleteReminder {
                eid: format!("eid-{i}"),
            })
            .collect();
        let result = check_bulk_deletion(&actions, 20, "Tasks", &dummy_state_path(), 25);
        assert!(matches!(result, Err(SyncError::SafetyAbort(_))));
    }

    #[test]
    fn bulk_delete_custom_percent_higher_threshold_passes_more() {
        // With 80%, tracked=10 → threshold=max(8,3)=8. 8 deletes ≤ 8 → passes.
        let actions: Vec<SyncAction> = (1..=8)
            .map(|i| SyncAction::DeleteReminder {
                eid: format!("eid-{i}"),
            })
            .collect();
        assert!(check_bulk_deletion(&actions, 10, "Tasks", &dummy_state_path(), 80).is_ok());
    }

    #[test]
    fn bulk_delete_error_message_includes_percent() {
        // Verify the error message mentions the configured percentage.
        let actions: Vec<SyncAction> = (1..=5)
            .map(|i| SyncAction::DeleteReminder {
                eid: format!("eid-{i}"),
            })
            .collect();
        let result = check_bulk_deletion(&actions, 6, "Tasks", &dummy_state_path(), 50);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("50%"),
            "error message should mention configured percentage: {err}"
        );
        assert!(
            err.contains("max_delete_percent"),
            "error message should hint at the config key: {err}"
        );
    }

    // ── check_task_count_coherence ────────────────────────────────────────────

    #[test]
    fn coherence_normal_passes() {
        // pre=10, post=8, deletes=2 → 8+2=10 >= 10 → passes
        assert!(check_task_count_coherence(10, 8, 2, "Tasks").is_ok());
    }

    #[test]
    fn coherence_tasks_added_passes() {
        // pre=10, post=12, deletes=0 → 12 >= 10 → passes
        assert!(check_task_count_coherence(10, 12, 0, "Tasks").is_ok());
    }

    #[test]
    fn coherence_silent_drop_aborts() {
        // pre=10, post=7, deletes=2 → 7+2=9 < 10 → SafetyAbort
        let result = check_task_count_coherence(10, 7, 2, "Tasks");
        assert!(matches!(result, Err(SyncError::SafetyAbort(_))));
    }

    #[test]
    fn coherence_zero_tasks_passes() {
        // pre=0, post=0, deletes=0 → 0 >= 0 → passes
        assert!(check_task_count_coherence(0, 0, 0, "Tasks").is_ok());
    }
}
