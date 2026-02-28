use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::error::SyncError;
use crate::sync::state::SyncState;

/// Resolve the sync state file path using the following priority chain:
///
/// 1. `$REMTODO_STATE_DIR/state.json` — if set, always use it (even if file doesn't exist).
/// 2. `$XDG_STATE_HOME/remtodo/state.json` — if `$XDG_STATE_HOME` is set and file exists.
/// 3. `~/.local/state/remtodo/state.json` — if the file exists.
/// 4. `~/Library/Application Support/remtodo/state.json` — if file exists (macOS native).
/// 5. Legacy `~/.local/state/ttdlsync/state.json` — if file exists (logs a deprecation warning).
/// 6. `~/.local/state/remtodo/state.json` — default for first-run.
pub fn resolve_state_path() -> Result<PathBuf, SyncError> {
    // Priority 1: explicit env var always wins (even for new state files).
    if let Ok(val) = std::env::var("REMTODO_STATE_DIR") {
        let p = PathBuf::from(&val).join("state.json");
        log::info!("State: using $REMTODO_STATE_DIR → {}", p.display());
        return Ok(p);
    }

    let home = dirs::home_dir().expect("cannot determine home directory");
    let default_path = home
        .join(".local")
        .join("state")
        .join("remtodo")
        .join("state.json");

    // Priority 2: $XDG_STATE_HOME if set and file exists.
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        let p = PathBuf::from(xdg).join("remtodo").join("state.json");
        if p.exists() {
            log::info!("State: found via $XDG_STATE_HOME → {}", p.display());
            return Ok(p);
        }
    }

    // Priority 3: ~/.local/state/remtodo/state.json
    if default_path.exists() {
        log::info!("State: found ~/.local/state/remtodo/state.json");
        return Ok(default_path);
    }

    // Priority 4: ~/Library/Application Support/remtodo/state.json (macOS native).
    if let Some(lib) = dirs::config_dir() {
        let p = lib.join("remtodo").join("state.json");
        if p.exists() {
            log::info!("State: found macOS native path → {}", p.display());
            return Ok(p);
        }
    }

    // Priority 5: legacy ~/.local/state/ttdlsync/state.json from before rename.
    let legacy = home
        .join(".local")
        .join("state")
        .join("ttdlsync")
        .join("state.json");
    if legacy.exists() {
        log::warn!(
            "State: found legacy path {} — please move it to ~/.local/state/remtodo/state.json",
            legacy.display()
        );
        return Ok(legacy);
    }

    // Priority 6: default first-run path.
    log::info!("State: no existing state found; defaulting to ~/.local/state/remtodo/state.json");
    Ok(default_path)
}

/// Returns the default path for the sync state file.
///
/// Deprecated: use [`resolve_state_path`] instead.
#[deprecated(since = "0.2.0", note = "use resolve_state_path instead")]
pub fn default_state_path() -> Result<PathBuf, SyncError> {
    resolve_state_path()
}

/// Load sync state from `path`.
///
/// Returns `Ok(None)` if the file does not exist, `Ok(Some(state))` on
/// success, or `Err` if the file is present but cannot be parsed.
///
/// If a `{path}.tmp` file exists, the previous sync was interrupted before
/// the atomic rename completed. A warning is logged and the stale temp file
/// is removed so it does not interfere with this run.
pub fn load_state(path: &Path) -> Result<Option<SyncState>, SyncError> {
    // Detect and clean up a leftover temp file from an interrupted sync.
    let tmp_path = tmp_path(path);
    if tmp_path.exists() {
        log::warn!(
            "Found leftover {p} — the previous sync was interrupted before state could be saved. \
             The state file ({state}) reflects the last successfully completed sync.",
            p = tmp_path.display(),
            state = path.display(),
        );
        let _ = fs::remove_file(&tmp_path);
    }

    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(path)?;
    let state: SyncState = serde_json::from_str(&data)?;
    Ok(Some(state))
}

/// Save sync state to `path` atomically: write to `{path}.tmp` first, then
/// rename into place. This guarantees that a crash mid-write never produces
/// a corrupt state file — either the old file survives intact or the new one
/// is fully written.
pub fn save_state(path: &Path, state: &SyncState) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    let json = serde_json::to_string_pretty(state)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut p = path.to_path_buf();
    let ext = p
        .extension()
        .map(|e| format!("{}.tmp", e.to_string_lossy()))
        .unwrap_or_else(|| "tmp".to_string());
    p.set_extension(ext);
    p
}

/// Return the modification time of `path` as a UTC `NaiveDateTime`,
/// or `None` if the file does not exist or the mtime cannot be read.
pub fn file_mtime_utc(path: &Path) -> Option<NaiveDateTime> {
    let meta = fs::metadata(path).ok()?;
    let mtime: SystemTime = meta.modified().ok()?;
    let dt: DateTime<Utc> = mtime.into();
    Some(dt.naive_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SyncError;
    use crate::sync::state::{SyncItemState, SyncState, SyncedFieldState};
    use chrono::NaiveDateTime;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn make_state() -> SyncState {
        let now =
            NaiveDateTime::parse_from_str("2026-02-25 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let mut items = HashMap::new();
        items.insert(
            "eid-1".to_string(),
            SyncItemState {
                eid: "eid-1".to_string(),
                fields: SyncedFieldState {
                    title: "Buy milk".to_string(),
                    priority: 0,
                    is_completed: false,
                    completion_date: None,
                    due_date: Some("2026-03-01".to_string()),
                    notes: None,
                    list: "Shopping".to_string(),
                },
                reminders_last_modified: Some(now),
                task_line_hash: 12345,
                reminders_field_hash: 99999,
                last_synced: now,
                pushed: true,
            },
        );
        SyncState {
            items,
            last_sync_time: Some(now),
        }
    }

    #[test]
    fn roundtrip_serialization() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");

        let state = make_state();
        save_state(&path, &state).unwrap();

        let loaded = load_state(&path).unwrap().expect("state should be present");
        assert_eq!(loaded.last_sync_time, state.last_sync_time);
        assert_eq!(loaded.items.len(), 1);
        let item = loaded.items.get("eid-1").unwrap();
        assert_eq!(item.eid, "eid-1");
        assert_eq!(item.fields.title, "Buy milk");
        assert_eq!(item.task_line_hash, 12345);
        assert_eq!(item.reminders_field_hash, 99999);
    }

    #[test]
    fn reminders_field_hash_defaults_to_zero_for_old_state() {
        // Old state.json files without reminders_field_hash must deserialize
        // with the field defaulting to 0, which is treated as "unknown" (changed)
        // so the surviving side always wins — conservative, no accidental deletes.
        let json = r#"{
            "items": {
                "eid-old": {
                    "eid": "eid-old",
                    "fields": {
                        "title": "Old task",
                        "priority": 0,
                        "is_completed": false,
                        "completion_date": null,
                        "due_date": null,
                        "notes": null,
                        "list": "Tasks"
                    },
                    "reminders_last_modified": null,
                    "task_line_hash": 42,
                    "last_synced": "2026-02-25T10:00:00"
                }
            },
            "last_sync_time": null
        }"#;
        let state: SyncState = serde_json::from_str(json).expect("should deserialize old JSON");
        let item = state.items.get("eid-old").unwrap();
        assert_eq!(
            item.reminders_field_hash, 0,
            "missing reminders_field_hash must default to 0"
        );
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(load_state(&path).unwrap().is_none());
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("state.json");
        let state = SyncState::default();
        save_state(&path, &state).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn mtime_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ghost.json");
        assert!(file_mtime_utc(&path).is_none());
    }

    #[test]
    fn mtime_existing_file_returns_some() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.json");
        fs::write(&path, "{}").unwrap();
        assert!(file_mtime_utc(&path).is_some());
    }

    #[test]
    fn save_state_no_tmp_left_behind() {
        // After a successful save, the .tmp file must not exist.
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        save_state(&path, &SyncState::default()).unwrap();
        assert!(path.exists(), "state.json should exist");
        assert!(
            !dir.path().join("state.json.tmp").exists(),
            ".tmp should be gone after rename"
        );
    }

    #[test]
    fn load_state_cleans_up_stale_tmp() {
        // A leftover .tmp file should be silently removed; load should succeed.
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        let tmp = dir.path().join("state.json.tmp");

        // Write a valid state file and a stale .tmp.
        save_state(&path, &make_state()).unwrap();
        fs::write(&tmp, "garbage from interrupted write").unwrap();

        assert!(tmp.exists());
        let loaded = load_state(&path).unwrap().expect("should load OK");
        assert_eq!(loaded.items.len(), 1, "real state should be returned");
        assert!(!tmp.exists(), ".tmp should have been removed");
    }

    #[test]
    fn load_state_malformed_json_returns_parse_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "{ this is not valid json }").unwrap();
        let result = load_state(&path);
        assert!(
            matches!(result, Err(SyncError::JsonParse(_))),
            "expected JsonParse error, got {result:?}"
        );
    }

    #[test]
    fn save_state_readonly_parent_returns_io_error() {
        use std::os::unix::fs::PermissionsExt;

        // Skip if running as root — root can write to read-only directories.
        let is_root = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim() == "0")
            .unwrap_or(false);
        if is_root {
            return;
        }

        let dir = tempdir().unwrap();
        let readonly_dir = dir.path().join("readonly");
        fs::create_dir(&readonly_dir).unwrap();
        fs::set_permissions(&readonly_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let path = readonly_dir.join("state.json");
        let result = save_state(&path, &SyncState::default());

        // Restore permissions so tempdir cleanup can remove the directory.
        let _ = fs::set_permissions(&readonly_dir, fs::Permissions::from_mode(0o755));

        assert!(
            matches!(result, Err(SyncError::Io(_))),
            "expected Io error when writing to read-only directory, got {result:?}"
        );
    }

    // ── resolve_state_path ────────────────────────────────────────────────────

    /// Helper: run a closure with env vars set, restoring originals on exit.
    /// Holds the process-wide ENV_LOCK so parallel tests don't race on env mutations.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let saved: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        f();
        for (k, orig) in &saved {
            match orig {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn resolve_state_path_env_var_override() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join("custom-state");
        let expected = state_dir.join("state.json");
        let dir_str = state_dir.to_string_lossy().to_string();

        // File does not need to exist — REMTODO_STATE_DIR always wins.
        with_env(
            &[
                ("REMTODO_STATE_DIR", Some(&dir_str)),
                ("XDG_STATE_HOME", None),
            ],
            || {
                let result = resolve_state_path().unwrap();
                assert_eq!(
                    result, expected,
                    "REMTODO_STATE_DIR must override all other paths"
                );
            },
        );
    }

    #[test]
    fn resolve_state_path_xdg_state_home() {
        let tmp = tempdir().unwrap();
        // Create a state file under the XDG dir.
        let xdg_state = tmp.path().join("xdg-state");
        let state_file = xdg_state.join("remtodo").join("state.json");
        fs::create_dir_all(state_file.parent().unwrap()).unwrap();
        fs::write(&state_file, "{}").unwrap();
        let xdg_str = xdg_state.to_string_lossy().to_string();

        with_env(
            &[
                ("REMTODO_STATE_DIR", None),
                ("XDG_STATE_HOME", Some(&xdg_str)),
                ("HOME", Some(&tmp.path().to_string_lossy())),
            ],
            || {
                let result = resolve_state_path().unwrap();
                assert_eq!(result, state_file, "should find state via XDG_STATE_HOME");
            },
        );
    }

    #[test]
    fn resolve_state_path_xdg_default_fallthrough() {
        // No env vars, no existing files → default path.
        let tmp = tempdir().unwrap();
        let home_str = tmp.path().to_string_lossy().to_string();

        with_env(
            &[
                ("REMTODO_STATE_DIR", None),
                ("XDG_STATE_HOME", None),
                ("HOME", Some(&home_str)),
            ],
            || {
                let result = resolve_state_path().unwrap();
                let s = result.to_string_lossy();
                assert!(
                    s.ends_with("remtodo/state.json"),
                    "fallthrough should resolve to remtodo/state.json, got: {s}"
                );
                assert!(
                    !s.contains("ttdlsync"),
                    "fallthrough must not resolve to legacy path, got: {s}"
                );
            },
        );
    }

    #[test]
    fn resolve_state_path_finds_legacy_dir() {
        let tmp = tempdir().unwrap();
        let legacy_state = tmp
            .path()
            .join(".local")
            .join("state")
            .join("ttdlsync")
            .join("state.json");
        fs::create_dir_all(legacy_state.parent().unwrap()).unwrap();
        fs::write(&legacy_state, "{}").unwrap();
        let home_str = tmp.path().to_string_lossy().to_string();

        with_env(
            &[
                ("REMTODO_STATE_DIR", None),
                ("XDG_STATE_HOME", None),
                ("HOME", Some(&home_str)),
            ],
            || {
                let result = resolve_state_path().unwrap();
                assert_eq!(
                    result, legacy_state,
                    "should find legacy ~/.local/state/ttdlsync/state.json"
                );
            },
        );
    }
}
