# remtodo

Keep Apple Reminders and your todo.txt file in sync — Rust daemon using
TTDL's todo_lib + Swift EventKit bridge.

[![CI](https://github.com/83noit/remtodo/actions/workflows/ci.yml/badge.svg)](https://github.com/83noit/remtodo/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

---

## What it does

`remtodo` keeps an [Apple Reminders](https://www.apple.com/reminders/) list and
a [todo.txt](http://todotxt.org/) file in sync, bidirectionally. Edit a task in
Reminders on your iPhone — it appears in your `todo.txt`. Complete a task in
[TTDL](https://github.com/VladimirMarkelov/ttdl) — it's marked done in Reminders.

---

## Prerequisites

- **macOS 13 (Ventura) or later**
- **Xcode Command Line Tools** — `xcode-select --install`
  (full Xcode required only to run `swift test`)
- **Rust toolchain (stable)** — [rustup](https://rustup.rs/) or via `nix develop`
- **TTDL** *(optional)* — `remtodo` works with any standard todo.txt file;
  [TTDL](https://github.com/VladimirMarkelov/ttdl) is one way to manage it

---

## Installation

Both binaries must live in the same directory — `remtodo` locates
`reminders-helper` as a sibling.

**With nix develop:**

```bash
# Step 1 — outside nix develop: Swift requires the system toolchain
cd swift && swift build -c release && cd ..

# Step 2 — build Rust and copy both binaries to ~/.local/bin
nix develop -c make install
```

**With rustup:**

```bash
# Build the Swift EventKit helper (system Swift)
cd swift && swift build -c release && cd ..

# Build the Rust binary
cargo build --release

# Copy both to your PATH
cp target/release/remtodo ~/.local/bin/
cp swift/.build/release/reminders-helper ~/.local/bin/
```

On first run, macOS will prompt for Reminders access in
**System Settings → Privacy & Security → Reminders**.

---

## Configuration

Create `~/.config/remtodo/config.toml`:

```toml
# Path to your todo.txt file
output = "~/Notes/Tasks/todo.txt"

# Include completed reminders on import (default: false)
include_completed = false

# Sync interval in seconds when running as a launchd agent (default: 60)
poll_interval_secs = 60

# Safety guard: abort if more than this % of tracked reminders would be
# deleted in one cycle (default: 50; set to 100 to disable)
max_delete_percent = 50

# Timestamp tolerance in seconds for LWW conflict resolution (default: 0).
# Set to 1–2 if you see spurious "reminder wins" decisions on iCloud.
timestamp_tolerance_secs = 0

# One [[lists]] block per Reminders list to sync.
[[lists]]
reminders_list = "Tasks"

# Stamp @work on imported tasks and push @work tasks back to this list.
# auto_context = "work"

# TTDL filter controlling which tasks are pushed to this list.
# Overrides auto_context for push selection when set.
# push_filter = "@work~due=any"   # @work OR any due date

# Sticky tracking: "auto" (default), "always", or "never"
# auto   — release tasks when they change and no list matches
# always — never release once tracked
# never  — release immediately when push filter no longer matches
# sticky_tracking = "auto"

# Priority mapping: Reminders integer → todo.txt representation.
# Default: 9 (low) → @today. Values: "context:NAME", "priority:A"–"Z", "none".
# priority_map = { "1" = "priority:A", "5" = "priority:B", "9" = "context:today" }

# Per-field writeback: set false to make todo.txt authoritative for that field.
# [lists.writeback]
# due_date = false
# priority = false

# [[lists]]
# reminders_list = "Shopping"
# auto_context = "shopping"
```

Config is resolved in order: `$REMTODO_CONFIG` →
`$XDG_CONFIG_HOME/remtodo/config.toml` → `~/.config/remtodo/config.toml` →
`~/Library/Application Support/remtodo/config.toml`.

---

## Usage

```bash
# One-shot sync
remtodo sync

# Preview without writing anything
remtodo sync --dry-run

# Use a specific config file
remtodo sync --config ~/path/to/config.toml

# Install as a launchd agent (auto-starts on login, polls every poll_interval_secs)
remtodo install
remtodo install --config ~/path/to/config.toml

# Show agent status and recent log output
remtodo status

# Remove the launchd agent
remtodo uninstall

# Undo the last sync — reverts Reminders mutations and restores todo.txt
# and state.json from pre-sync backups. Reminder failures are logged but
# do not abort the restore.
remtodo restore
```

---

## Architecture

```
┌─────────────────┐     JSON over stdout      ┌──────────────────────┐
│  remtodo (Rust) │ ────────────────────────▶ │  reminders-helper    │
│                 │                           │  (Swift, EventKit)   │
│  sync engine    │ ◀─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ │                      │
│  todo_lib crate │       reminder JSON        └──────────────────────┘
│  launchd agent  │
└────────┬────────┘
         │ read / write
         ▼
    todo.txt file
```

`remtodo` is a Rust binary that owns the sync logic and reads/writes the
todo.txt file via TTDL's [`todo_lib` crate](https://crates.io/crates/todo_lib).
It spawns `reminders-helper` — a small Swift CLI — as a subprocess to access
Apple Reminders through the native EventKit framework. The helper outputs a JSON
array of reminders; `remtodo` computes a three-way diff against the last known
state (`state.json`) and applies the minimum set of changes to both sides.

Pre-sync backups are written alongside `state.json` so `remtodo restore` can
undo the last cycle.

---

## Limitations

- **macOS only** — depends on Apple Reminders and the EventKit framework.
- **Recurrence not synced** — EventKit recurrence rules are ignored on import;
  no recurrence rule is set when pushing to Reminders. If you use `rec:` in
  todo.txt, TTDL manages recurrence locally and `remtodo` propagates
  completions.
- **iCloud EID reassignment** — when iCloud reassigns a reminder's external
  identifier (e.g. after a device restore), `remtodo` treats it as a deletion +
  new reminder. Tracked for a future release.
- **launchd agent label** — the bundled label `me.83noit.remtodo.agent` is a
  personal reverse-DNS identifier. If you install the agent you may want to
  customise it.

---

## Acknowledgements

- [`todo_lib`](https://crates.io/crates/todo_lib) by
  [Vladimir Markelov](https://github.com/VladimirMarkelov) —
  todo.txt parsing and serialization
