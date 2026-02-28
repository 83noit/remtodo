use thiserror::Error;

#[derive(Error, Debug)]
pub enum SyncError {
    #[error("Swift CLI error: {0}")]
    SwiftCli(String),

    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Todo file error: {0}")]
    Todo(#[from] todo_lib::terr::TodoError),

    #[error("Config error: {0}")]
    Config(String),

    /// Aborted because a safety invariant was violated.
    ///
    /// Includes a human-readable explanation and a remediation hint.
    #[error("Safety check failed — aborting to prevent data loss: {0}")]
    SafetyAbort(String),

    /// Another `remtodo sync` process is already running.
    #[error(
        "Another sync process (PID {0}) is already running. \
         If that process is stuck, remove the lock file manually."
    )]
    LockConflict(u32),

    /// Sync was interrupted by SIGINT/SIGTERM; state was saved up to this point.
    #[error(
        "Sync interrupted — state saved for {0} completed list(s). \
         Re-run to finish."
    )]
    Interrupted(usize),
}
