use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::SyncError;
use crate::sync::config::ListSyncConfig;

fn default_poll_interval() -> u64 {
    60
}

fn default_max_delete_percent() -> u8 {
    50
}

fn default_timestamp_tolerance_secs() -> u64 {
    0
}

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub output: String,
    #[serde(default)]
    pub include_completed: bool,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Maximum percentage of tracked reminders that may be deleted in a single
    /// sync before the bulk-deletion safety guard fires.  Default: 50.
    /// Set to 100 to disable the guard entirely (not recommended).
    #[serde(default = "default_max_delete_percent")]
    pub max_delete_percent: u8,
    /// Tolerance window (seconds) for mtime comparison in the three-way diff.
    ///
    /// EventKit and HFS+ both round timestamps to 1-second precision; CalDAV
    /// implementations vary.  Setting this to 1 or 2 prevents spurious
    /// reminder-wins decisions caused by sub-second rounding.  Default: 0
    /// (strict — existing behaviour).
    #[serde(default = "default_timestamp_tolerance_secs")]
    pub timestamp_tolerance_secs: u64,
    pub lists: Vec<ListSyncConfig>,
}

/// Resolve the config file path using the following priority chain:
///
/// 1. `$REMTODO_CONFIG` — if set, always use it (even if the file doesn't exist yet).
/// 2. `$XDG_CONFIG_HOME/remtodo/config.toml` — if `$XDG_CONFIG_HOME` is set and the file exists.
/// 3. `~/.config/remtodo/config.toml` — if the file exists.
/// 4. `~/Library/Application Support/remtodo/config.toml` — if the file exists (macOS native).
/// 5. Legacy `~/.config/ttdlsync/config.toml` — if the file exists (logs a deprecation warning).
/// 6. `~/.config/remtodo/config.toml` — default for first-run (caller creates the file).
pub fn resolve_config_path() -> PathBuf {
    // Priority 1: explicit env var always wins.
    if let Ok(val) = std::env::var("REMTODO_CONFIG") {
        let p = PathBuf::from(&val);
        log::info!("Config: using $REMTODO_CONFIG → {}", p.display());
        return p;
    }

    let home = dirs::home_dir().expect("cannot determine home directory");

    // Priority 2: $XDG_CONFIG_HOME if set and file exists.
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg).join("remtodo").join("config.toml");
        if p.exists() {
            log::info!("Config: found via $XDG_CONFIG_HOME → {}", p.display());
            return p;
        }
    }

    // Priority 3: ~/.config/remtodo/config.toml
    let dot_config = home.join(".config").join("remtodo").join("config.toml");
    if dot_config.exists() {
        log::info!("Config: found ~/.config/remtodo/config.toml");
        return dot_config;
    }

    // Priority 4: ~/Library/Application Support/remtodo/config.toml (macOS native).
    if let Some(lib) = dirs::config_dir() {
        let p = lib.join("remtodo").join("config.toml");
        if p.exists() {
            log::info!("Config: found macOS native path → {}", p.display());
            return p;
        }
    }

    // Priority 5: legacy ~/.config/ttdlsync/config.toml from before rename.
    let legacy = home.join(".config").join("ttdlsync").join("config.toml");
    if legacy.exists() {
        log::warn!(
            "Config: found legacy path {} — please move it to ~/.config/remtodo/config.toml",
            legacy.display()
        );
        return legacy;
    }

    // Priority 6: default first-run path.
    log::info!("Config: no existing config found; defaulting to ~/.config/remtodo/config.toml");
    dot_config
}

pub fn load_config(path: &Path) -> Result<AppConfig, SyncError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| SyncError::Config(format!("Cannot read {}: {}", path.display(), e)))?;
    toml::from_str(&content)
        .map_err(|e| SyncError::Config(format!("Parse error in {}: {}", path.display(), e)))
}

pub fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}/{}", home.display(), rest);
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SyncError;
    use tempfile::TempDir;

    // Helper: run a closure with env vars set, restoring originals on exit.
    // Holds the process-wide ENV_LOCK so parallel tests don't race on env mutations.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        // Save originals.
        let saved: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        // Apply.
        for (k, v) in vars {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        f();
        // Restore.
        for (k, orig) in &saved {
            match orig {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn resolve_config_path_env_var_override() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("my-config.toml");
        // File does not need to exist — REMTODO_CONFIG always wins.
        let cfg_str = cfg.to_string_lossy().to_string();

        with_env(
            &[
                ("REMTODO_CONFIG", Some(&cfg_str)),
                ("XDG_CONFIG_HOME", None),
            ],
            || {
                let result = resolve_config_path();
                assert_eq!(result, cfg, "REMTODO_CONFIG must override all other paths");
            },
        );
    }

    #[test]
    fn resolve_config_path_xdg_default_fallthrough() {
        // When $XDG_CONFIG_HOME is set but no file exists there, and no file exists
        // at ~/.config/remtodo/config.toml, ~/Library, or the legacy path,
        // fall through to the default ~/.config/remtodo/config.toml.
        let tmp = TempDir::new().unwrap();
        let xdg_home = tmp.path().join("xdg"); // dir exists but no config inside
        let home_dir = tmp.path().to_string_lossy().to_string();
        let xdg_str = xdg_home.to_string_lossy().to_string();

        with_env(
            &[
                ("REMTODO_CONFIG", None),
                ("XDG_CONFIG_HOME", Some(&xdg_str)),
                ("HOME", Some(&home_dir)),
            ],
            || {
                let result = resolve_config_path();
                // Should end with remtodo/config.toml (not legacy, not Library)
                let s = result.to_string_lossy();
                assert!(
                    s.ends_with("remtodo/config.toml"),
                    "fallthrough should resolve to remtodo/config.toml, got: {s}"
                );
                assert!(
                    !s.contains("ttdlsync"),
                    "fallthrough must not resolve to legacy path, got: {s}"
                );
            },
        );
    }

    #[test]
    fn resolve_config_path_finds_legacy_dir() {
        let tmp = TempDir::new().unwrap();
        // Create a fake home structure with only the legacy path present.
        let legacy_cfg = tmp
            .path()
            .join(".config")
            .join("ttdlsync")
            .join("config.toml");
        std::fs::create_dir_all(legacy_cfg.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_cfg,
            "output = \"/tmp/todo.txt\"\n[[lists]]\nreminders_list = \"Tasks\"\n",
        )
        .unwrap();

        // Override HOME so dirs::home_dir() returns our tmp dir.
        // Note: dirs uses the HOME env var on Unix.
        with_env(
            &[
                ("REMTODO_CONFIG", None),
                ("XDG_CONFIG_HOME", None),
                ("HOME", Some(&tmp.path().to_string_lossy())),
            ],
            || {
                let result = resolve_config_path();
                assert_eq!(
                    result, legacy_cfg,
                    "should find legacy ~/.config/ttdlsync/config.toml"
                );
            },
        );
    }

    #[test]
    fn expand_tilde_absolute_path_unchanged() {
        let p = "/home/user/Notes/Tasks/todo.txt";
        assert_eq!(expand_tilde(p), p);
    }

    #[test]
    fn expand_tilde_expands_home() {
        let expanded = expand_tilde("~/foo/bar.txt");
        assert!(
            !expanded.starts_with("~/"),
            "tilde should have been expanded: {expanded}"
        );
        assert!(expanded.ends_with("/foo/bar.txt"));
    }

    #[test]
    fn load_config_parses_valid_toml() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"
output = "/tmp/todo.txt"
[[lists]]
reminders_list = "Tasks"
"#
        )
        .unwrap();

        let cfg = load_config(f.path()).expect("should parse");
        assert_eq!(cfg.output, "/tmp/todo.txt");
        assert_eq!(cfg.lists.len(), 1);
        assert_eq!(cfg.lists[0].reminders_list, "Tasks");
    }

    #[test]
    fn load_config_missing_file_returns_config_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.toml");
        let result = load_config(&path);
        assert!(
            matches!(result, Err(SyncError::Config(_))),
            "expected Config error, got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Cannot read"),
            "error message should mention 'Cannot read', got: {msg}"
        );
    }

    #[test]
    fn load_config_invalid_toml_returns_config_error() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        write!(f, "this is not ][ valid {{ toml").unwrap();
        let result = load_config(f.path());
        assert!(
            matches!(result, Err(SyncError::Config(_))),
            "expected Config error, got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Parse error"),
            "error message should mention 'Parse error', got: {msg}"
        );
    }

    #[test]
    fn load_config_writeback_partial_defaults_to_true() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"
output = "/tmp/todo.txt"
[[lists]]
reminders_list = "Tasks"

[lists.writeback]
due_date = false
"#
        )
        .unwrap();

        let cfg = load_config(f.path()).expect("should parse");
        let wb = &cfg.lists[0].writeback;
        assert!(wb.title, "title should default to true");
        assert!(!wb.due_date, "due_date should be false as configured");
        assert!(wb.priority, "priority should default to true");
        assert!(wb.is_completed, "is_completed should default to true");
    }
}
