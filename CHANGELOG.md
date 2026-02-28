# Changelog

All notable changes to this project will be documented in this file.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.0.1] - 2026-02-28

### Changed

- Bump `dirs` 5 → 6, `signal-hook` 0.3 → 0.4, `toml` 0.8 → 1.0
- Bump `actions/checkout` v4 → v6 in CI

## [1.0.0] - 2026-02-28

Initial public release.

### Sync engine

- Bidirectional sync between Apple Reminders and todo.txt via a three-way
  diff against persisted state (`state.json`)
- Last-write-wins (LWW) conflict resolution per field, with configurable
  timestamp tolerance (`timestamp_tolerance_secs`) to absorb iCloud rounding
- Hash-based change detection on both sides (no spurious updates)
- Bootstrap reconciliation by title + due date on first sync, so existing
  tasks and reminders are linked without duplication
- EID relinking: detects iCloud external-identifier reassignment and
  re-links by content hash instead of treating it as delete + create
- Recurrence: EventKit recurrence rules ignored on import; TTDL manages
  `rec:` recurrence locally and `remtodo` propagates completions

### Sentinel `eid:` values

Three reserved tags let tasks opt out of sync or trigger ejection:

- `eid:na` — permanent local opt-out, never pushed to Reminders
- `eid:na/<orig>` — eject a synced reminder and keep task local
- `eid:ns/<orig>` — eject a synced reminder and resume normal rules

### Configuration

- Per-list sync configuration: `auto_context`, `push_filter`, `sticky_tracking`
- Configurable priority mapping: Reminders integers → `context:NAME`,
  `priority:A`–`Z`, or `none` (default: priority 9 → `@today`)
- Per-field writeback control: set `false` to make todo.txt authoritative
  for `title`, `due_date`, `priority`, or `is_completed`
- Sticky tracking modes: `auto` (origin-aware release), `always`, `never`

### Safety guards

- Bulk-delete threshold: aborts if more than `max_delete_percent` (default 50%)
  of tracked reminders would be deleted in one cycle
- First-sync protection: no deletions on the first sync for a list
- Task-count coherence check: aborts if the output file shrinks unexpectedly
- Post-sync consistency verification: detects duplicate EIDs and hash mismatches

### Operations

- `remtodo sync` — one-shot sync with optional `--dry-run` and `--config`
- `remtodo restore` — reverts Reminders mutations and restores todo.txt +
  state.json from pre-sync backups
- `remtodo install` / `status` / `uninstall` — launchd agent management
- Lock file (PID-based) prevents concurrent sync runs
- Graceful SIGINT/SIGTERM handling with per-list rollback

### Infrastructure

- Swift EventKit helper (`reminders-helper`) with batch create/update/delete
- `make install` builds the Rust binary inside `nix develop` and copies both
  binaries to `~/.local/bin`; Swift must be pre-built outside `nix develop`
- nix flake dev environment (`nix develop`)
- GitHub Actions CI: `cargo fmt`, `cargo clippy`, `cargo test`, `swift build`
- Dependabot for Cargo and GitHub Actions updates
