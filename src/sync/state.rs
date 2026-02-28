use std::collections::HashMap;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

/// Per-field snapshot recorded at last sync, used for three-way diff.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq)]
pub struct SyncedFieldState {
    pub title: String,
    pub priority: i32,
    pub is_completed: bool,
    pub completion_date: Option<String>,
    pub due_date: Option<String>,
    pub notes: Option<String>,
    pub list: String,
}

/// Per-item sync metadata persisted between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncItemState {
    pub eid: String,
    /// Field snapshot from last sync (baseline for three-way diff).
    pub fields: SyncedFieldState,
    /// When the reminder was last modified, as reported by EventKit.
    pub reminders_last_modified: Option<NaiveDateTime>,
    /// Hash of the task's todo.txt line at last sync (eid: tag stripped).
    pub task_line_hash: u64,
    /// Hash of the reminder's synced fields at last sync.
    #[serde(default)]
    pub reminders_field_hash: u64,
    /// Wall-clock time of the last successful sync for this item.
    pub last_synced: NaiveDateTime,
    /// Whether this relationship was created by pushing a task to Reminders
    /// (`true`) or by pulling a reminder into todo.txt / inbox (`false`).
    /// Defaults to `false` for old state files where origin is unknown —
    /// this is the conservative choice (don't auto-release items of unknown origin).
    #[serde(default)]
    pub pushed: bool,
}

/// Full sync state persisted to disk between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub items: HashMap<String, SyncItemState>,
    pub last_sync_time: Option<NaiveDateTime>,
}
