pub mod config;
pub mod error;
pub mod filter;
pub mod launchd;
pub mod lock;
pub mod mapping;
pub mod reminder;
pub mod swift_cli;
pub mod sync;
pub mod undo;

/// Process-wide mutex used by tests that mutate env vars.
///
/// Tests that call `std::env::set_var` / `std::env::remove_var` must hold this
/// lock for the duration, because env mutations are not thread-safe when tests
/// run in parallel.
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
