//! Exclusive sync lock — prevents two `remtodo sync` processes from running
//! concurrently against the same state directory.
//!
//! A plain text file containing the owner's PID is written to
//! `{state_dir}/sync.lock` at the start of each sync and removed on drop.
//! If the file already exists and its PID is still alive, the new process
//! aborts with [`SyncError::LockConflict`]. If the PID is gone (stale lock),
//! the file is overwritten with a warning.

use std::fs;
use std::path::{Path, PathBuf};

use log::warn;

use crate::error::SyncError;

/// RAII sync lock. Acquired by [`SyncLock::acquire`]; released on drop.
pub struct SyncLock {
    path: PathBuf,
}

impl SyncLock {
    const FILENAME: &'static str = "sync.lock";

    /// Try to acquire the sync lock in `state_dir`.
    ///
    /// - If no lock file exists, creates one and returns `Ok(SyncLock)`.
    /// - If a lock file exists with a live PID, returns
    ///   `Err(SyncError::LockConflict(pid))`.
    /// - If a lock file exists with a dead PID (stale lock), logs a warning,
    ///   removes the stale file, and acquires the lock.
    pub fn acquire(state_dir: &Path) -> Result<Self, SyncError> {
        let path = state_dir.join(Self::FILENAME);

        if path.exists() {
            let content = fs::read_to_string(&path).unwrap_or_default();
            let pid: u32 = content.trim().parse().unwrap_or(0);
            if pid > 0 && pid_is_alive(pid) {
                return Err(SyncError::LockConflict(pid));
            }
            warn!(
                "Removing stale sync lock (PID {pid} is no longer running): {}",
                path.display()
            );
            let _ = fs::remove_file(&path);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, std::process::id().to_string())?;
        Ok(SyncLock { path })
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Returns `true` if a process with `pid` is currently alive.
///
/// Uses the POSIX `kill -0` signal (no signal is actually sent; it only checks
/// whether the process exists and we have permission to signal it).
fn pid_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
