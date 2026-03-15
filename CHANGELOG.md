# Changelog

All notable changes to this project will be documented in this file.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.2.0] - 2026-03-15

### Changed

- **Upgrade `todo_lib` to v11**: the dependency now tracks the same major
  version as TTDL itself, ensuring field parsing and task serialisation stay
  in sync with the upstream library.

- **`push_filter` now backed by `todo_lib::flt`**: the custom filter parser
  (~670 lines) has been replaced with a thin wrapper around the `flt` module
  that ships with `todo_lib` v11.  The public API and all existing config
  strings are unchanged; shorthand normalisations (`@ctx`, `+prj`, `#tag`,
  `-@ctx`, `-+prj`) and `+Nd`/`+Nw` date-offset syntax continue to work as
  before.  The upgrade brings the full upstream filter DSL for free: hashtag
  conditions (`#=tag`), subject-text search (`subj=`), custom key/value tag
  filters (e.g. `rec=any`), and threshold-date comparisons (`t=..today`).

### Fixed

- **Date-sensitive recurrence test**: the `recurring_completion_spawns_next_instance`
  unit test used a hard-coded past due date; `todo_lib`'s `done()` advances
  strict recurrences past today, so the expected date drifted as calendar time
  passed.  The test now uses a far-future due date to remain deterministic.

## [1.1.2] - 2026-03-04

### Fixed

- **Duplicate `eid:` on TTDL-spawned recurrence instances**: completing a
  recurring task via `ttdl done` directly caused `todo_lib`'s
  `cleanup_cloned_task()` to inherit the parent's `eid:` on the spawned next
  instance (only `tmr:` and `spent:` are stripped by the library).  The
  previous fix in v1.1.0 covered completions triggered *by* `remtodo sync`
  but not external completions.  A pre-sync dedup pass now detects tasks
  sharing an `eid:`, keeps it on the baseline copy (matched against the
  stored `task_line_hash`), and strips it from duplicates so the spawn
  receives a fresh Reminder on the next sync.  Recurring tasks may now be
  completed freely via either `remtodo sync` or `ttdl done`.

## [1.1.1] - 2026-03-03

### Added

- `remtodo --version` / `-V` / `version` — print the version string and exit.

### Fixed

- **Orphan `eid:` tags after Triage release**: when `sticky_tracking = "triage"`
  released a task from Reminders, the state entry was removed but the `eid:`
  tag was left on the task in todo.txt.  On every subsequent sync this produced
  a `verify_post_sync` "no state entry" warning.  The fix strips `eid:` from
  the task at release time; a one-time cleanup pass also heals tasks left in
  this state by the initial v1.1.0 run.

- **Hash mismatch warnings for untracked field changes**: `task_line_hash`
  covers the full task line, but `three_way_diff` only tracks five synced
  fields (title, due date, priority, completion status, completion date).
  Editing an untracked field — adding or removing a context, project, `rec:`
  tag, or any other custom tag — produced a `verify_post_sync` "hash mismatch"
  warning on every cycle, and could cause an incorrect `ResurrectReminder`
  instead of `DeleteTask` in Case B.  A hash-reconciliation pass at the end
  of each action cycle now keeps the stored hash accurate.

## [1.1.0] - 2026-03-03

### Changed

- **`sticky_tracking` mode `"auto"` renamed to `"triage"`** — the old `auto`
  mode distinguished between push-origin and pull-origin tasks: pull-origin
  tasks (created in Reminders, pulled into todo.txt) would only be released
  from Reminders if another configured list admitted them after an edit.
  This caused reminders to persist indefinitely for the common inbox workflow
  (Reminders → todo.txt → prioritise/edit → filter governs).  The new
  `triage` mode uses a simpler rule: any edit to a task in todo.txt is the
  triage signal; once edited, the push filter is authoritative.  Unedited
  tasks retain their inbox protection regardless of origin.
  **Migration:** replace `sticky_tracking = "auto"` with
  `sticky_tracking = "triage"` in `config.toml`; `"auto"` is now a parse
  error.

### Fixed

- Recurring tasks: the spawned next instance no longer inherits the parent's
  `eid:` tag.  Previously `todo_lib`'s `cleanup_cloned_task()` did not strip
  `eid:`, so the completed parent and the new instance shared the same EID.
  This caused `verify_post_sync` to report a duplicate-EID warning on every
  subsequent sync cycle.

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
- Sticky tracking modes: `triage` (edit-triggered release), `always`, `never`

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
