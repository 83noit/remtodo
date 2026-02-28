use std::fs;
use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::error::SyncError;
use crate::reminder::Reminder;
use crate::swift_cli::{CreateReminderInput, SwiftCli};
use crate::sync::actions::ReminderUpdate;

/// A log of all reminder mutations that succeeded during a sync pass.
///
/// Written to disk before any file writes; used by `restore` to undo.
#[derive(Debug, Serialize, Deserialize)]
pub struct UndoLog {
    /// ISO 8601 timestamp of the sync that created this log.
    pub timestamp: String,
    /// Absolute path of the todo file that was backed up.
    pub todo_original_path: String,
    /// Ordered list of mutations to reverse (apply in order to undo).
    pub entries: Vec<UndoEntry>,
}

/// A single reversible reminder mutation.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UndoEntry {
    /// A reminder was created during sync → delete it to undo.
    UndoCreate { eid: String, list_name: String },
    /// A reminder was deleted during sync → recreate it to undo.
    UndoDelete { reminder: Reminder },
    /// A reminder was updated during sync → revert all fields to undo.
    UndoUpdate { old_reminder: Reminder },
}

/// Returns the path of the undo log: `state_dir/undo.json`.
pub fn undo_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("undo.json")
}

/// Returns `(todo.md.bak, state.json.bak)` paths within `state_dir`.
pub fn backup_file_paths(state_dir: &Path) -> (PathBuf, PathBuf) {
    (
        state_dir.join("todo.md.bak"),
        state_dir.join("state.json.bak"),
    )
}

/// Copy `todo_path` and `state_path` into `state_dir` as `.bak` files.
///
/// Missing source files are silently skipped.
pub fn create_pre_sync_backup(
    todo_path: &Path,
    state_path: &Path,
    state_dir: &Path,
) -> Result<(), SyncError> {
    fs::create_dir_all(state_dir)?;
    let (todo_bak, state_bak) = backup_file_paths(state_dir);

    if todo_path.exists() {
        fs::copy(todo_path, &todo_bak)?;
        info!("Backed up {} → {}", todo_path.display(), todo_bak.display());
    }

    if state_path.exists() {
        fs::copy(state_path, &state_bak)?;
        info!(
            "Backed up {} → {}",
            state_path.display(),
            state_bak.display()
        );
    }

    Ok(())
}

/// Persist the undo log to `state_dir/undo.json`.
pub fn save_undo_log(state_dir: &Path, log: &UndoLog) -> Result<(), SyncError> {
    fs::create_dir_all(state_dir)?;
    let path = undo_log_path(state_dir);
    let json = serde_json::to_string_pretty(log)?;
    fs::write(&path, json)?;
    Ok(())
}

/// Load the undo log from `state_dir/undo.json`.
///
/// Returns an error if the file is missing.
pub fn load_undo_log(state_dir: &Path) -> Result<UndoLog, SyncError> {
    let path = undo_log_path(state_dir);
    if !path.exists() {
        return Err(SyncError::Config(
            "No backup to restore from — undo.json not found. Run `remtodo sync` first."
                .to_string(),
        ));
    }
    let data = fs::read_to_string(&path)?;
    let log: UndoLog = serde_json::from_str(&data)?;
    Ok(log)
}

/// Build a `ReminderUpdate` that reverts all mutable fields to `r`'s values.
fn revert_update(r: &Reminder) -> ReminderUpdate {
    ReminderUpdate {
        eid: r.external_id.clone(),
        list_name: r.list.clone(),
        title: Some(r.title.clone()),
        priority: Some(r.priority),
        is_completed: Some(r.is_completed),
        completion_date: Some(r.completion_date.clone()),
        due_date: Some(r.due_date.clone()),
        notes: Some(r.notes.clone()),
    }
}

/// Build a `CreateReminderInput` from a full `Reminder` snapshot.
fn recreate_input(r: &Reminder) -> CreateReminderInput {
    CreateReminderInput {
        title: r.title.clone(),
        list_name: r.list.clone(),
        priority: r.priority,
        due_date: r.due_date.clone(),
        notes: r.notes.clone(),
        is_completed: r.is_completed,
        completion_date: r.completion_date.clone(),
    }
}

/// Execute all undo entries best-effort, restore file backups, and clean up.
///
/// Individual reminder API failures are logged as warnings and do not abort.
/// Returns an error only if the undo log cannot be loaded or file restoration fails.
pub fn execute_restore(cli: &SwiftCli, state_dir: &Path) -> Result<(), SyncError> {
    let log = load_undo_log(state_dir)?;
    info!("Restoring from backup created at {}", log.timestamp);

    let mut failures = 0usize;

    for entry in &log.entries {
        match entry {
            UndoEntry::UndoCreate { eid, list_name } => match cli.delete_reminder(eid, list_name) {
                Ok(()) => info!("Undo: deleted created reminder eid:{eid}"),
                Err(e) => {
                    warn!("Undo: failed to delete reminder eid:{eid}: {e}");
                    failures += 1;
                }
            },
            UndoEntry::UndoDelete { reminder } => {
                let input = recreate_input(reminder);
                match cli.create_reminder(&input) {
                    Ok(r) => info!(
                        "Undo: recreated deleted reminder '{}' as eid:{}",
                        reminder.title, r.external_id
                    ),
                    Err(e) => {
                        warn!(
                            "Undo: failed to recreate reminder '{}': {e}",
                            reminder.title
                        );
                        failures += 1;
                    }
                }
            }
            UndoEntry::UndoUpdate { old_reminder } => {
                let update = revert_update(old_reminder);
                match cli.update_reminder(&update) {
                    Ok(_) => info!(
                        "Undo: reverted reminder '{}' eid:{}",
                        old_reminder.title, old_reminder.external_id
                    ),
                    Err(e) => {
                        warn!(
                            "Undo: failed to revert reminder eid:{}: {e}",
                            old_reminder.external_id
                        );
                        failures += 1;
                    }
                }
            }
        }
    }

    // Restore file backups.
    let todo_path = Path::new(&log.todo_original_path);
    let (todo_bak, state_bak) = backup_file_paths(state_dir);
    let state_path = state_dir.join("state.json");

    if todo_bak.exists() {
        fs::copy(&todo_bak, todo_path)?;
        info!("Restored {} → {}", todo_bak.display(), todo_path.display());
    }

    if state_bak.exists() {
        fs::copy(&state_bak, &state_path)?;
        info!(
            "Restored {} → {}",
            state_bak.display(),
            state_path.display()
        );
    }

    // Clean up backup files.
    let _ = fs::remove_file(undo_log_path(state_dir));
    let _ = fs::remove_file(&todo_bak);
    let _ = fs::remove_file(&state_bak);

    if failures == 0 {
        info!("Restore complete.");
    } else {
        info!(
            "Restore partially complete — {failures} reminder operation(s) failed (see warnings above)."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::reminder::Reminder;

    fn make_reminder(eid: &str) -> Reminder {
        Reminder {
            id: format!("id-{eid}"),
            external_id: eid.to_string(),
            title: "Test reminder".to_string(),
            due_date: Some("2026-03-01".to_string()),
            priority: 5,
            is_completed: false,
            completion_date: None,
            creation_date: Some("2026-02-20".to_string()),
            last_modified_date: Some("2026-02-25T10:00:00Z".to_string()),
            notes: Some("Test note".to_string()),
            list: "Tasks".to_string(),
        }
    }

    // ── create_pre_sync_backup ────────────────────────────────────────────────

    #[test]
    fn backup_copies_both_files() {
        let tmp = TempDir::new().unwrap();
        let todo_path = tmp.path().join("todo.txt");
        let state_path = tmp.path().join("state.json");
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(&todo_path, b"task content").unwrap();
        fs::write(&state_path, b"state content").unwrap();

        create_pre_sync_backup(&todo_path, &state_path, &state_dir).unwrap();

        let (todo_bak, state_bak) = backup_file_paths(&state_dir);
        assert_eq!(fs::read(&todo_bak).unwrap(), b"task content");
        assert_eq!(fs::read(&state_bak).unwrap(), b"state content");
    }

    #[test]
    fn backup_skips_missing_files() {
        let tmp = TempDir::new().unwrap();
        let todo_path = tmp.path().join("todo.txt");
        let state_path = tmp.path().join("state.json"); // intentionally absent
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(&todo_path, b"task content").unwrap();

        create_pre_sync_backup(&todo_path, &state_path, &state_dir).unwrap();

        let (todo_bak, state_bak) = backup_file_paths(&state_dir);
        assert!(todo_bak.exists());
        assert!(!state_bak.exists()); // state was absent — no backup created
    }

    #[test]
    fn backup_creates_state_dir() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("nonexistent");
        let no_todo = tmp.path().join("todo.txt"); // absent
        let no_state = tmp.path().join("state.json"); // absent
        assert!(!state_dir.exists());

        create_pre_sync_backup(&no_todo, &no_state, &state_dir).unwrap();

        assert!(state_dir.exists());
    }

    // ── save_undo_log / load_undo_log ─────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let log = UndoLog {
            timestamp: "2026-02-27T12:00:00Z".to_string(),
            todo_original_path: "/home/user/todo.txt".to_string(),
            entries: vec![
                UndoEntry::UndoCreate {
                    eid: "eid-create".to_string(),
                    list_name: "Tasks".to_string(),
                },
                UndoEntry::UndoDelete {
                    reminder: make_reminder("eid-delete"),
                },
                UndoEntry::UndoUpdate {
                    old_reminder: make_reminder("eid-update"),
                },
            ],
        };

        save_undo_log(tmp.path(), &log).unwrap();
        let loaded = load_undo_log(tmp.path()).unwrap();

        // Compare via JSON serialization (UndoLog does not derive PartialEq)
        let orig_json = serde_json::to_string(&log).unwrap();
        let loaded_json = serde_json::to_string(&loaded).unwrap();
        assert_eq!(orig_json, loaded_json);
    }

    #[test]
    fn load_missing_returns_error() {
        let tmp = TempDir::new().unwrap();
        let result = load_undo_log(tmp.path());
        assert!(matches!(result, Err(SyncError::Config(_))));
    }

    #[test]
    fn save_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("new_state_dir");
        assert!(!state_dir.exists());

        let log = UndoLog {
            timestamp: "2026-02-27T12:00:00Z".to_string(),
            todo_original_path: "/tmp/todo.txt".to_string(),
            entries: vec![],
        };
        save_undo_log(&state_dir, &log).unwrap();

        assert!(undo_log_path(&state_dir).exists());
    }

    // ── private helpers ───────────────────────────────────────────────────────

    #[test]
    fn revert_update_populates_all_fields() {
        let r = Reminder {
            id: "id-1".to_string(),
            external_id: "eid-1".to_string(),
            title: "Buy milk".to_string(),
            due_date: Some("2026-03-01".to_string()),
            priority: 9,
            is_completed: true,
            completion_date: Some("2026-02-28".to_string()),
            creation_date: None,
            last_modified_date: None,
            notes: Some("2% from Costco".to_string()),
            list: "Shopping".to_string(),
        };

        let update = revert_update(&r);

        assert_eq!(update.eid, "eid-1");
        assert_eq!(update.list_name, "Shopping");
        assert_eq!(update.title, Some("Buy milk".to_string()));
        assert_eq!(update.priority, Some(9));
        assert_eq!(update.is_completed, Some(true));
        assert_eq!(update.completion_date, Some(Some("2026-02-28".to_string())));
        assert_eq!(update.due_date, Some(Some("2026-03-01".to_string())));
        assert_eq!(update.notes, Some(Some("2% from Costco".to_string())));
    }

    #[test]
    fn recreate_input_populates_all_fields() {
        let r = Reminder {
            id: "id-1".to_string(),
            external_id: "eid-1".to_string(),
            title: "Buy milk".to_string(),
            due_date: Some("2026-03-01".to_string()),
            priority: 5,
            is_completed: false,
            completion_date: None,
            creation_date: None,
            last_modified_date: None,
            notes: Some("organic".to_string()),
            list: "Shopping".to_string(),
        };

        let input = recreate_input(&r);

        assert_eq!(input.title, "Buy milk");
        assert_eq!(input.list_name, "Shopping");
        assert_eq!(input.priority, 5);
        assert_eq!(input.due_date, Some("2026-03-01".to_string()));
        assert_eq!(input.notes, Some("organic".to_string()));
        assert!(!input.is_completed);
        assert!(input.completion_date.is_none());
    }
}
