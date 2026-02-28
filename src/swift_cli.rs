use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::SyncError;
use crate::reminder::{Reminder, ReminderList};
use crate::sync::actions::ReminderUpdate;

/// A single operation for the `batch` subcommand.
///
/// Serialised as an internally-tagged JSON object:
///   `{"op": "create-reminder", "title": "...", "listName": "...", ...}`
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op")]
pub enum BatchOp {
    #[serde(rename = "create-reminder")]
    CreateReminder(CreateReminderInput),
    #[serde(rename = "update-reminder")]
    UpdateReminder(ReminderUpdate),
    #[serde(rename = "delete-reminder")]
    DeleteReminder {
        eid: String,
        #[serde(rename = "listName")]
        list_name: String,
    },
}

/// Per-operation result returned by the `batch` subcommand.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItemResult {
    pub ok: bool,
    /// Present on successful create-reminder / update-reminder.
    pub reminder: Option<Reminder>,
    /// Present on successful delete-reminder.
    pub deleted: Option<bool>,
    /// Human-readable error message when `ok = false`.
    pub error: Option<String>,
}

/// Input payload for the `create-reminder` Swift CLI command.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReminderInput {
    pub title: String,
    pub list_name: String,
    pub priority: i32,
    pub due_date: Option<String>,
    pub notes: Option<String>,
    pub is_completed: bool,
    pub completion_date: Option<String>,
}

/// Wrapper around the Swift `reminders-helper` CLI binary.
pub struct SwiftCli {
    binary: PathBuf,
}

impl SwiftCli {
    pub fn new() -> Result<Self, SyncError> {
        let binary = Self::find_binary()?;
        Ok(Self { binary })
    }

    /// Locate the reminders-helper binary.
    ///
    /// Search order:
    /// 1. `REMINDERS_HELPER` environment variable
    /// 2. Sibling of the current executable
    /// 3. Swift build output (release then debug)
    /// 4. `PATH`
    fn find_binary() -> Result<PathBuf, SyncError> {
        // 1. Environment variable
        if let Ok(path) = std::env::var("REMINDERS_HELPER") {
            let p = PathBuf::from(&path);
            if p.exists() {
                return Ok(p);
            }
        }

        // 2. Sibling of current executable (covers ~/.local/bin installs and launchd)
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let sibling = dir.join("reminders-helper");
                if sibling.exists() {
                    return Ok(sibling);
                }
            }
        }

        // 3. PATH lookup
        if let Ok(output) = Command::new("which").arg("reminders-helper").output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Ok(PathBuf::from(path));
                }
            }
        }

        Err(SyncError::SwiftCli(
            "reminders-helper not found. Install with: make install".to_string(),
        ))
    }

    /// Spawn a subcommand, write `input` to its stdin, and return stdout bytes.
    fn run_with_stdin(&self, subcommand: &str, input: &[u8]) -> Result<Vec<u8>, SyncError> {
        let mut child = Command::new(&self.binary)
            .arg(subcommand)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input)?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SyncError::SwiftCli(stderr.to_string()));
        }
        Ok(output.stdout)
    }

    /// Execute multiple reminder-side operations in a single process spawn.
    ///
    /// Passes a JSON array of `BatchOp` values to the `batch` subcommand and
    /// returns a parallel array of `BatchItemResult` values.  The Swift CLI
    /// defers the EventKit commit until all operations have been processed,
    /// then flushes once — reducing round-trips to the Reminders store.
    ///
    /// **Partial failure:** individual results can have `ok = false` without
    /// rolling back the operations that succeeded.  Callers must inspect each
    /// `BatchItemResult::ok` field and handle failures individually.  The Rust
    /// engine counts per-action failures and gates the final write accordingly.
    ///
    /// Returns `Err` only if the process itself fails, the output is malformed,
    /// or the result count doesn't match the input count.
    pub fn batch(&self, ops: &[BatchOp]) -> Result<Vec<BatchItemResult>, SyncError> {
        if ops.is_empty() {
            return Ok(vec![]);
        }
        let json = serde_json::to_vec(ops)?;
        let output = self.run_with_stdin("batch", &json)?;
        let results: Vec<BatchItemResult> = serde_json::from_slice(&output)?;
        if results.len() != ops.len() {
            return Err(SyncError::SwiftCli(format!(
                "batch: expected {} result(s), got {}",
                ops.len(),
                results.len()
            )));
        }
        Ok(results)
    }

    /// Create a reminder and return the result (including the system-assigned eid).
    pub fn create_reminder(&self, input: &CreateReminderInput) -> Result<Reminder, SyncError> {
        let json = serde_json::to_vec(input)?;
        let output = self.run_with_stdin("create-reminder", &json)?;
        let reminder: Reminder = serde_json::from_slice(&output)?;
        Ok(reminder)
    }

    /// Apply a partial update to an existing reminder and return the updated state.
    pub fn update_reminder(&self, update: &ReminderUpdate) -> Result<Reminder, SyncError> {
        let json = serde_json::to_vec(update)?;
        let output = self.run_with_stdin("update-reminder", &json)?;
        let reminder: Reminder = serde_json::from_slice(&output)?;
        Ok(reminder)
    }

    /// Delete a reminder by eid.
    pub fn delete_reminder(&self, eid: &str, list_name: &str) -> Result<(), SyncError> {
        let body = serde_json::json!({ "eid": eid, "listName": list_name });
        let json = serde_json::to_vec(&body)?;
        self.run_with_stdin("delete-reminder", &json)?;
        Ok(())
    }

    /// Create a Reminders list by name (idempotent).
    pub fn create_list(&self, name: &str) -> Result<ReminderList, SyncError> {
        let body = serde_json::json!({ "title": name });
        let json = serde_json::to_vec(&body)?;
        let output = self.run_with_stdin("create-list", &json)?;
        let list: ReminderList = serde_json::from_slice(&output)?;
        Ok(list)
    }

    /// Delete a Reminders list by name (idempotent).
    pub fn delete_list(&self, name: &str) -> Result<(), SyncError> {
        let body = serde_json::json!({ "title": name });
        let json = serde_json::to_vec(&body)?;
        self.run_with_stdin("delete-list", &json)?;
        Ok(())
    }

    /// List all Reminders lists.
    #[allow(dead_code)] // Will be used by future CLI commands
    pub fn list_lists(&self) -> Result<Vec<ReminderList>, SyncError> {
        let output = Command::new(&self.binary).arg("list-lists").output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SyncError::SwiftCli(stderr.to_string()));
        }

        let lists: Vec<ReminderList> = serde_json::from_slice(&output.stdout)?;
        Ok(lists)
    }

    /// Fetch reminders from a named list.
    pub fn get_reminders(
        &self,
        list_name: &str,
        include_completed: bool,
    ) -> Result<Vec<Reminder>, SyncError> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("get-reminders").arg("--list").arg(list_name);

        if include_completed {
            cmd.arg("--include-completed");
        }

        let output = cmd.output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SyncError::SwiftCli(stderr.to_string()));
        }

        let reminders: Vec<Reminder> = serde_json::from_slice(&output.stdout)?;
        Ok(reminders)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SyncError;
    use crate::sync::actions::ReminderUpdate;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Write `content` as an executable shell script inside `dir` and return its path.
    fn make_script(dir: &TempDir, content: &str) -> PathBuf {
        let path = dir.path().join("fake-helper");
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Construct a `SwiftCli` backed by `script` without going through `find_binary`.
    ///
    /// Because this test module lives inside `swift_cli`, it can access the
    /// private `binary` field directly.
    fn cli(script: &std::path::Path) -> SwiftCli {
        SwiftCli {
            binary: script.to_path_buf(),
        }
    }

    fn minimal_create_input() -> CreateReminderInput {
        CreateReminderInput {
            title: "Test task".to_string(),
            list_name: "Tasks".to_string(),
            priority: 0,
            due_date: None,
            notes: None,
            is_completed: false,
            completion_date: None,
        }
    }

    fn minimal_update() -> ReminderUpdate {
        ReminderUpdate {
            eid: "test-eid".to_string(),
            list_name: "Tasks".to_string(),
            title: None,
            priority: None,
            is_completed: None,
            completion_date: None,
            due_date: None,
            notes: None,
        }
    }

    // ── get_reminders ─────────────────────────────────────────────────────────

    #[test]
    fn get_reminders_nonzero_exit_is_swift_cli_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(&dir, "#!/bin/sh\necho 'access denied' >&2\nexit 1\n");
        let result = cli(&script).get_reminders("Tasks", false);
        assert!(
            matches!(result, Err(SyncError::SwiftCli(_))),
            "expected SwiftCli error, got {result:?}"
        );
    }

    #[test]
    fn get_reminders_stderr_captured_in_error_message() {
        let dir = TempDir::new().unwrap();
        let script = make_script(&dir, "#!/bin/sh\necho 'permission denied' >&2\nexit 1\n");
        let err = cli(&script).get_reminders("Tasks", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("permission denied"),
            "stderr should appear in the error message, got: {msg}"
        );
    }

    #[test]
    fn get_reminders_invalid_json_is_parse_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(&dir, "#!/bin/sh\necho 'not valid json'\n");
        let result = cli(&script).get_reminders("Tasks", false);
        assert!(
            matches!(result, Err(SyncError::JsonParse(_))),
            "expected JsonParse error, got {result:?}"
        );
    }

    // ── create_reminder ───────────────────────────────────────────────────────

    #[test]
    fn create_reminder_nonzero_exit_is_swift_cli_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(&dir, "#!/bin/sh\necho 'list not found' >&2\nexit 2\n");
        let result = cli(&script).create_reminder(&minimal_create_input());
        assert!(
            matches!(result, Err(SyncError::SwiftCli(_))),
            "expected SwiftCli error, got {result:?}"
        );
    }

    #[test]
    fn create_reminder_invalid_json_is_parse_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(
            &dir,
            "#!/bin/sh\ncat /dev/stdin >/dev/null\necho 'not json'\n",
        );
        let result = cli(&script).create_reminder(&minimal_create_input());
        assert!(
            matches!(result, Err(SyncError::JsonParse(_))),
            "expected JsonParse error, got {result:?}"
        );
    }

    // ── update_reminder ───────────────────────────────────────────────────────

    #[test]
    fn update_reminder_nonzero_exit_is_swift_cli_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(&dir, "#!/bin/sh\necho 'reminder not found' >&2\nexit 1\n");
        let result = cli(&script).update_reminder(&minimal_update());
        assert!(
            matches!(result, Err(SyncError::SwiftCli(_))),
            "expected SwiftCli error, got {result:?}"
        );
    }

    // ── list_lists ────────────────────────────────────────────────────────────

    #[test]
    fn list_lists_nonzero_exit_is_swift_cli_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(
            &dir,
            "#!/bin/sh\necho 'reminders unavailable' >&2\nexit 1\n",
        );
        let result = cli(&script).list_lists();
        assert!(
            matches!(result, Err(SyncError::SwiftCli(_))),
            "expected SwiftCli error, got {result:?}"
        );
    }

    #[test]
    fn list_lists_invalid_json_is_parse_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(&dir, "#!/bin/sh\necho 'not json'\n");
        let result = cli(&script).list_lists();
        assert!(
            matches!(result, Err(SyncError::JsonParse(_))),
            "expected JsonParse error, got {result:?}"
        );
    }

    // ── batch ─────────────────────────────────────────────────────────────────

    fn minimal_batch_ops() -> Vec<BatchOp> {
        vec![BatchOp::DeleteReminder {
            eid: "test-eid".to_string(),
            list_name: "Tasks".to_string(),
        }]
    }

    #[test]
    fn batch_empty_ops_returns_empty_vec_without_spawning() {
        // No fake script needed — batch() short-circuits for empty input.
        let dir = TempDir::new().unwrap();
        // Script that always fails: if called, it would fail the test.
        let script = make_script(&dir, "#!/bin/sh\nexit 1\n");
        let result = cli(&script).batch(&[]);
        assert!(result.is_ok(), "expected Ok for empty ops, got {result:?}");
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn batch_nonzero_exit_is_swift_cli_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(
            &dir,
            "#!/bin/sh\ncat /dev/stdin >/dev/null\necho 'batch failed' >&2\nexit 1\n",
        );
        let result = cli(&script).batch(&minimal_batch_ops());
        assert!(
            matches!(result, Err(SyncError::SwiftCli(_))),
            "expected SwiftCli error, got {result:?}"
        );
    }

    #[test]
    fn batch_invalid_json_is_parse_error() {
        let dir = TempDir::new().unwrap();
        let script = make_script(
            &dir,
            "#!/bin/sh\ncat /dev/stdin >/dev/null\necho 'not json'\n",
        );
        let result = cli(&script).batch(&minimal_batch_ops());
        assert!(
            matches!(result, Err(SyncError::JsonParse(_))),
            "expected JsonParse error, got {result:?}"
        );
    }

    #[test]
    fn batch_result_count_mismatch_is_swift_cli_error() {
        // Script returns 0 results but we sent 1 op → mismatch.
        let dir = TempDir::new().unwrap();
        let script = make_script(&dir, "#!/bin/sh\ncat /dev/stdin >/dev/null\necho '[]'\n");
        let result = cli(&script).batch(&minimal_batch_ops());
        assert!(
            matches!(result, Err(SyncError::SwiftCli(_))),
            "expected SwiftCli error for count mismatch, got {result:?}"
        );
    }

    #[test]
    fn batch_success_returns_parsed_results() {
        let dir = TempDir::new().unwrap();
        // Script echoes one result matching the one op we send.
        let script = make_script(
            &dir,
            r#"#!/bin/sh
cat /dev/stdin >/dev/null
echo '[{"ok":true,"deleted":true}]'
"#,
        );
        let results = cli(&script).batch(&minimal_batch_ops()).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok);
        assert_eq!(results[0].deleted, Some(true));
        assert!(results[0].reminder.is_none());
        assert!(results[0].error.is_none());
    }

    #[test]
    fn batch_op_serialisation_create_reminder() {
        // Verify that CreateReminder serialises with "op": "create-reminder"
        // and camelCase field names matching the Swift CLI expectation.
        let op = BatchOp::CreateReminder(CreateReminderInput {
            title: "Buy milk".to_string(),
            list_name: "Tasks".to_string(),
            priority: 0,
            due_date: Some("2026-03-15".to_string()),
            notes: None,
            is_completed: false,
            completion_date: None,
        });
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "create-reminder");
        assert_eq!(json["title"], "Buy milk");
        assert_eq!(json["listName"], "Tasks");
        assert_eq!(json["priority"], 0);
        assert_eq!(json["dueDate"], "2026-03-15");
        assert_eq!(json["isCompleted"], false);
    }

    #[test]
    fn batch_op_serialisation_delete_reminder() {
        let op = BatchOp::DeleteReminder {
            eid: "test-eid-123".to_string(),
            list_name: "Shopping".to_string(),
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "delete-reminder");
        assert_eq!(json["eid"], "test-eid-123");
        assert_eq!(json["listName"], "Shopping");
    }
}
