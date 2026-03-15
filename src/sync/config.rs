use std::collections::HashMap;

use serde::de::{self, Deserializer, Visitor};
use todo_lib::todotxt::Task;

use crate::filter::Filter;

// ============================================================
// Sticky tracking mode
// ============================================================

/// Controls whether tasks that fall off the push filter are released from sync.
///
/// - `Always` (or `true` in config): tasks are never released once tracked.
/// - `Triage` (default, `"triage"` in config): tasks are released when they
///   have been edited in todo.txt and no longer match the owning list's push
///   filter. Unedited tasks (hash unchanged) are protected — they stay in
///   Reminders regardless of filter drift.  This is the intended workflow:
///   pull from Reminders → triage/edit in todo.txt → filter governs.
/// - `Never` (or `false` in config): no sticky — release immediately on filter
///   miss, with no task-change protection.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum StickyTracking {
    /// Never release once tracked. Backward-compatible with `true`.
    Always,
    /// Edit-triggered release: any todo.txt edit on an off-filter task releases
    /// it from Reminders. Unedited tasks are protected (inbox safety).
    #[default]
    Triage,
    /// No sticky: tasks that fall off push_filter are immediately released.
    Never,
}

impl<'de> de::Deserialize<'de> for StickyTracking {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct StickyVisitor;

        impl<'de> Visitor<'de> for StickyVisitor {
            type Value = StickyTracking;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, r#"true, false, "always", "triage", or "never""#)
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<StickyTracking, E> {
                Ok(if v {
                    StickyTracking::Always
                } else {
                    StickyTracking::Never
                })
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<StickyTracking, E> {
                match v.to_ascii_lowercase().as_str() {
                    "always" => Ok(StickyTracking::Always),
                    "triage" => Ok(StickyTracking::Triage),
                    "never" => Ok(StickyTracking::Never),
                    _ => Err(E::unknown_variant(
                        v,
                        &["always", "triage", "never", "true", "false"],
                    )),
                }
            }
        }

        d.deserialize_any(StickyVisitor)
    }
}

// ============================================================
// Priority mapping
// ============================================================

/// What a Reminders priority level looks like on the todo.txt side.
#[derive(Debug, Clone, PartialEq)]
pub enum MappingTarget {
    /// Add `@<name>` context to the task (e.g. `Context("today")` → `@today`).
    Context(String),
    /// Set letter priority on the task (0 = A, 1 = B, …, 25 = Z).
    Priority(u8),
    /// No todo.txt representation — priority is silently dropped.
    Nothing,
}

impl MappingTarget {
    /// Parse a config string such as `"context:today"`, `"priority:A"`, or `"none"`.
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.eq_ignore_ascii_case("none") || s.is_empty() {
            return Ok(Self::Nothing);
        }
        if let Some(ctx) = s.strip_prefix("context:") {
            if ctx.is_empty() {
                return Err("context name must not be empty".to_string());
            }
            return Ok(Self::Context(ctx.to_string()));
        }
        if let Some(pri) = s.strip_prefix("priority:") {
            if let Some(c) = pri.chars().next() {
                if c.is_ascii_uppercase() {
                    return Ok(Self::Priority(c as u8 - b'A'));
                }
            }
            return Err(format!("invalid priority letter in '{s}': expected A–Z"));
        }
        Err(format!(
            "unrecognised mapping target '{s}': \
             expected 'context:NAME', 'priority:A'–'priority:Z', or 'none'"
        ))
    }
}

/// Bidirectional priority mapping for one Reminders list.
///
/// Maps Reminders priority integers (0, 1, 5, 9, …) to todo.txt
/// representations and back. The reverse (todo.txt → Reminders) is
/// derived from the same table automatically.
#[derive(Debug, Clone)]
pub struct PriorityMap {
    entries: Vec<(i32, MappingTarget)>,
}

impl Default for PriorityMap {
    /// Default mapping matches Phase-1 behaviour: Reminders priority 9 → `@today`.
    fn default() -> Self {
        Self {
            entries: vec![(9, MappingTarget::Context("today".to_string()))],
        }
    }
}

impl PriorityMap {
    /// Build a `PriorityMap` from the raw config table.
    ///
    /// Keys are Reminders priority integers (as strings); values are
    /// `"context:NAME"`, `"priority:A"`, or `"none"`.
    /// Malformed entries are logged as warnings and skipped.
    pub fn from_config(map: &HashMap<String, String>) -> Self {
        let mut entries = Vec::new();
        for (k, v) in map {
            match k.parse::<i32>() {
                Ok(priority) => match MappingTarget::parse(v) {
                    Ok(target) => entries.push((priority, target)),
                    Err(e) => log::warn!("priority_map: skipping invalid value '{v}': {e}"),
                },
                Err(_) => log::warn!("priority_map: skipping non-integer key '{k}'"),
            }
        }
        entries.sort_by_key(|(p, _)| *p);
        Self { entries }
    }

    /// Convert a Reminders priority value to its todo.txt representation.
    pub fn reminders_to_task(&self, reminders_priority: i32) -> &MappingTarget {
        self.entries
            .iter()
            .find(|(p, _)| *p == reminders_priority)
            .map(|(_, t)| t)
            .unwrap_or(&MappingTarget::Nothing)
    }

    /// Derive the Reminders priority from a task (inverse mapping).
    ///
    /// Scans entries for a context or letter-priority match.
    /// Returns `0` (no priority) if nothing matches.
    pub fn task_to_reminders(&self, task: &Task) -> i32 {
        for (reminders_pri, target) in &self.entries {
            match target {
                MappingTarget::Context(ctx) if task.contexts.contains(ctx) => {
                    return *reminders_pri;
                }
                MappingTarget::Priority(p) if task.priority == *p => {
                    return *reminders_pri;
                }
                _ => {}
            }
        }
        0
    }

    /// All context strings this map can produce (used to clean up old values).
    pub fn all_mapped_contexts(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|(_, t)| {
                if let MappingTarget::Context(ctx) = t {
                    Some(ctx.as_str())
                } else {
                    None
                }
            })
            .collect()
    }

    /// All letter priorities this map can produce (used to clean up old values).
    pub fn all_mapped_priorities(&self) -> Vec<u8> {
        self.entries
            .iter()
            .filter_map(|(_, t)| {
                if let MappingTarget::Priority(p) = t {
                    Some(*p)
                } else {
                    None
                }
            })
            .collect()
    }
}

// ============================================================
// Writeback control
// ============================================================

fn default_true() -> bool {
    true
}

/// Per-field writeback control: determines which Reminders→todo.txt field
/// updates are allowed.
///
/// When a field is `false`, the todo.txt side is authoritative: Reminders-only
/// changes for that field are suppressed and the task's current value is pushed
/// back to the Reminder instead. Default: all `true` (current behaviour).
///
/// Example TOML:
/// ```toml
/// [lists.writeback]
/// due_date = false   # todo.txt is authoritative for scheduling
/// priority = false   # todo.txt is authoritative for priority
/// ```
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct WritebackConfig {
    /// Allow Reminders title to overwrite the task subject. Default: `true`.
    #[serde(default = "default_true")]
    pub title: bool,
    /// Allow Reminders due date to overwrite the task due date. Default: `true`.
    #[serde(default = "default_true")]
    pub due_date: bool,
    /// Allow Reminders priority to overwrite the task priority. Default: `true`.
    #[serde(default = "default_true")]
    pub priority: bool,
    /// Allow Reminders completion status (and completion date) to overwrite the
    /// task. `completion_date` always follows this flag. Default: `true`.
    #[serde(default = "default_true")]
    pub is_completed: bool,
}

impl Default for WritebackConfig {
    fn default() -> Self {
        Self {
            title: true,
            due_date: true,
            priority: true,
            is_completed: true,
        }
    }
}

// ============================================================
// List sync configuration
// ============================================================

/// Configuration for syncing one Reminders list ↔ one todo.txt context.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ListSyncConfig {
    /// Name of the Apple Reminders list to sync.
    pub reminders_list: String,

    /// If set, stamps `@context` on tasks imported from this Reminders list,
    /// and (when no `push_filter` is set) restricts which tasks are pushed back.
    pub auto_context: Option<String>,

    /// TTDL-style filter expression controlling which todo.txt tasks are pushed
    /// to this Reminders list. When set, overrides `auto_context` for push
    /// selection (but `auto_context` still stamps imported tasks).
    ///
    /// Syntax: rules separated by `~` (OR); conditions within a rule separated
    /// by `;` (AND). Examples:
    ///   `@joint;due=..+2d`       — @joint AND due within 2 days
    ///   `@today~pri=any~due=any` — @today OR any priority OR any due date
    pub push_filter: Option<String>,

    /// Controls whether tasks that fall off the push filter are released from sync.
    ///
    /// - `"always"` (or `true`): never release once tracked (backward compat).
    /// - `"triage"` (default): release when the task has been edited in todo.txt
    ///   and no longer matches the push filter. Unedited tasks are protected.
    /// - `"never"` (or `false`): no sticky — release immediately on filter miss.
    #[serde(default)]
    pub sticky_tracking: StickyTracking,

    /// When `true`, tasks are created from Reminders that are already completed
    /// at first-sync time. When `false` (default), pre-completed reminders are
    /// skipped so historical completions are not imported.
    #[serde(default)]
    pub sync_initial_completed: bool,

    /// Per-list priority mapping. Keys are Reminders priority integers
    /// (e.g. `"9"` for low); values are `"context:NAME"`, `"priority:A"`,
    /// or `"none"`. When absent the default mapping applies
    /// (`9 → context:today`).
    ///
    /// Example (TOML inline table):
    /// ```toml
    /// priority_map = { "1" = "priority:A", "5" = "priority:B", "9" = "context:today" }
    /// ```
    #[serde(default)]
    pub priority_map: Option<HashMap<String, String>>,

    /// Per-field writeback control. When a field is `false`, Reminders→task
    /// updates for that field are suppressed and the task value is pushed back.
    /// Omitting this table entirely preserves the current all-enabled behaviour.
    #[serde(default)]
    pub writeback: WritebackConfig,
}

impl Default for ListSyncConfig {
    fn default() -> Self {
        Self {
            reminders_list: String::new(),
            auto_context: None,
            push_filter: None,
            sticky_tracking: StickyTracking::Triage,
            sync_initial_completed: false,
            priority_map: None,
            writeback: WritebackConfig::default(),
        }
    }
}

impl ListSyncConfig {
    pub fn new(reminders_list: impl Into<String>) -> Self {
        Self {
            reminders_list: reminders_list.into(),
            ..Default::default()
        }
    }

    pub fn with_auto_context(mut self, ctx: impl Into<String>) -> Self {
        self.auto_context = Some(ctx.into());
        self
    }

    pub fn with_writeback(mut self, wb: WritebackConfig) -> Self {
        self.writeback = wb;
        self
    }

    /// Parse and return the compiled push filter, if one is configured.
    ///
    /// Returns `None` when no `push_filter` is set (fall back to `auto_context`).
    /// An unrecognised filter string silently produces `Filter::deny_all()`.
    pub fn compiled_push_filter(&self) -> Option<Filter> {
        self.push_filter.as_deref().map(Filter::parse)
    }

    /// Return the compiled priority map for this list.
    ///
    /// Uses the list-specific `priority_map` config if set; otherwise falls back
    /// to the default (`9 → @today`).
    pub fn compiled_priority_map(&self) -> PriorityMap {
        match &self.priority_map {
            Some(map) => PriorityMap::from_config(map),
            None => PriorityMap::default(),
        }
    }
}
