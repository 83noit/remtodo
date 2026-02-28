use serde::Serialize;
use todo_lib::todotxt::Task;

use crate::reminder::Reminder;

/// A partial update to apply to an Apple Reminder.
///
/// `None` fields are left unchanged (omitted from JSON).
/// `Some(None)` clears the field (serialised as `null`).
/// `Some(Some(v))` sets the field to `v`.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReminderUpdate {
    pub eid: String,
    /// Reminders list name — needed by the Swift CLI for lookup.
    pub list_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_completed: Option<bool>,
    /// `None` = unchanged, `Some(None)` = clear, `Some(Some(v))` = set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_date: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<Option<String>>,
}

/// Decisions produced by the pure-function sync engine.
pub enum SyncAction {
    /// A new reminder has no corresponding task — create one.
    CreateTask { eid: String, reminder: Reminder },
    /// A new local task has no corresponding reminder — create one.
    CreateReminder { task: Task, target_list: String },
    /// The reminder changed since last sync — update the task.
    UpdateTask { eid: String, updated_task: Task },
    /// The task changed since last sync — update the reminder.
    UpdateReminder {
        eid: String,
        updated_reminder: ReminderUpdate,
    },
    /// The reminder was deleted — delete the task too.
    DeleteTask { eid: String },
    /// The task was deleted — delete the reminder too.
    DeleteReminder { eid: String },
    /// Both sides changed the same field to different values and timestamps
    /// don't clearly pick a winner — emit a merged result for both sides.
    // Phase 2: constructed by engine when timestamps are ambiguous.
    #[allow(dead_code)]
    MergeConflict {
        eid: String,
        updated_task: Task,
        updated_reminder: ReminderUpdate,
    },
    /// Task was deleted, but the reminder was modified more recently →
    /// re-create the task from the reminder.
    ResurrectTask { eid: String, task: Task },
    /// Reminder was deleted, but the task was modified more recently →
    /// re-create the reminder from the task.
    ResurrectReminder {
        eid: String,
        reminder_update: ReminderUpdate,
        target_list: String,
    },

    /// A task's `eid:na/<original>` sentinel is stale (the original eid is no
    /// longer in state, meaning the reminder was confirmed deleted in a prior
    /// sync).  Simplify the tag to plain `eid:na` so the task file stays tidy.
    CleanSentinelTag { sentinel_eid: String },

    /// iCloud reassigned a reminder's externalIdentifier. Rewrite the
    /// task's `eid:` tag and move the state entry from old to new key.
    /// No Reminders-side I/O — purely local bookkeeping.
    RelinkEid { old_eid: String, new_eid: String },
}
