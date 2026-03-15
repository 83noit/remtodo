//! TTDL-compatible push-filter expression parser and evaluator.
//!
//! Wraps `todo_lib::flt::Filter` with OR-group handling and shorthand
//! normalisation so that existing `push_filter` config strings remain valid:
//!
//! ```text
//! filter = rule ('~' rule)*        -- rules are OR-combined
//! rule   = cond (';' cond)*        -- conditions within a rule are AND-combined (flt native)
//! cond   = '@'ctx                  -- include context   (normalised → @=ctx)
//!        | '-@'ctx                 -- exclude context   (normalised → -@=ctx)
//!        | '+'prj                  -- include project   (normalised → +=prj)
//!        | '-+'prj                 -- exclude project   (normalised → -+=prj)
//!        | '#'tag                  -- include hashtag   (normalised → #=tag)
//!        | field '=' value         -- flt native conditions (passed through)
//! ```
//!
//! Date offsets: `+Nd`/`+Nw`/`+Nm` are normalised to `Nd`/`Nw`/`Nm` so
//! that `due=..+1d` works just like `due=..1d` in `flt`.
//!
//! Examples:
//!   `@joint;due=..1d`        — @joint context AND due within 1 day
//!   `@today~pri=any~due=any` — @today OR any priority OR any due date

use chrono::NaiveDate;
use todo_lib::flt;
use todo_lib::todotxt::Task;

/// A parsed push-filter expression.
///
/// Rules are OR-combined (split on `~` or `|`); conditions within a rule
/// are AND-combined (split on `;` by `flt::Filter::parse()`).
pub struct Filter {
    groups: Vec<flt::Filter>,
}

impl Filter {
    /// Parse a filter expression string (infallible — bad input produces empty groups).
    pub fn parse(s: &str) -> Self {
        let or_char = if s.contains('~') { '~' } else { '|' };
        let groups = s
            .split(or_char)
            .map(|g| g.trim())
            .filter(|g| !g.is_empty())
            .map(|g| flt::Filter::parse(&normalize_shorthands(g), false))
            .filter(|f| !f.is_empty())
            .collect();
        Filter { groups }
    }

    /// Returns a filter that never matches anything (used as a safe fallback).
    pub fn deny_all() -> Self {
        Filter { groups: vec![] }
    }

    /// Returns `true` if the task matches this filter, evaluated against `today`.
    pub fn matches(&self, task: &Task, today: NaiveDate) -> bool {
        self.groups.iter().any(|f| f.matches(task, 0, today))
    }
}

/// Normalise user-friendly shorthands to `flt::Filter` syntax.
///
/// Within each `;`-separated condition:
///   `@ctx`  → `@=ctx`     (context include)
///   `-@ctx` → `-@=ctx`    (context exclude)
///   `+prj`  → `+=prj`     (project include)
///   `-+prj` → `-+=prj`    (project exclude)
///   `#tag`  → `#=tag`     (hashtag include)
///
/// Conditions that already contain `=` are passed through with only date-offset
/// normalisation applied: `+Nd`/`+Nw`/`+Nm`/`+Ny` → `Nd`/`Nw`/`Nm`/`Ny`.
fn normalize_shorthands(group: &str) -> String {
    group
        .split(';')
        .map(|cond| {
            let cond = cond.trim();
            if cond.contains('=') {
                // Already has '=' (flt native or partially normalised).
                // Only normalise date offsets in the value part.
                return normalize_date_offsets(cond);
            }
            if let Some(rest) = cond.strip_prefix("-@") {
                format!("-@={rest}")
            } else if let Some(rest) = cond.strip_prefix('@') {
                format!("@={rest}")
            } else if let Some(rest) = cond.strip_prefix("-+") {
                format!("-+={rest}")
            } else if let Some(rest) = cond.strip_prefix('+') {
                format!("+={rest}")
            } else if let Some(rest) = cond.strip_prefix('-') {
                // e.g. `-#tag` is unusual but preserve negation
                if let Some(tag) = rest.strip_prefix('#') {
                    format!("-#={tag}")
                } else {
                    cond.to_string()
                }
            } else if let Some(rest) = cond.strip_prefix('#') {
                format!("#={rest}")
            } else {
                cond.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Strip leading `+` from date-offset patterns (`+Nd` → `Nd`) in value
/// positions so that `flt`'s `human_to_date` can parse them.
///
/// Only the value part (after the first `=`) is modified.
fn normalize_date_offsets(cond: &str) -> String {
    if let Some(eq_pos) = cond.find('=') {
        let (key, val) = cond.split_at(eq_pos + 1);
        format!("{key}{}", strip_plus_offsets(val))
    } else {
        cond.to_string()
    }
}

/// Replace `+<digits><unit>` with `<digits><unit>` where unit ∈ {d, w, m, y}.
fn strip_plus_offsets(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            // Skip the '+' — it's a date-offset prefix, not an operator
            i += 1;
            continue;
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use todo_lib::todotxt::Task;

    fn today() -> NaiveDate {
        chrono::Local::now().date_naive()
    }

    fn task_from(line: &str) -> Task {
        Task::parse(line, today())
    }

    fn task_ctx(ctx: &str) -> Task {
        task_from(&format!("task @{ctx}"))
    }

    fn task_pri(p: char) -> Task {
        task_from(&format!("({}) task", p.to_ascii_uppercase()))
    }

    fn task_due(days: i64) -> Task {
        let d = today() + Duration::days(days);
        task_from(&format!("task due:{}", d.format("%Y-%m-%d")))
    }

    // ── shorthand normalisation ───────────────────────────────────────────────

    #[test]
    fn normalize_context_shorthand() {
        assert_eq!(normalize_shorthands("@today"), "@=today");
        assert_eq!(normalize_shorthands("-@work"), "-@=work");
    }

    #[test]
    fn normalize_project_shorthand() {
        assert_eq!(normalize_shorthands("+shopping"), "+=shopping");
        assert_eq!(normalize_shorthands("-+shopping"), "-+=shopping");
    }

    #[test]
    fn normalize_hashtag_shorthand() {
        assert_eq!(normalize_shorthands("#foo"), "#=foo");
    }

    #[test]
    fn normalize_date_offset_in_due() {
        assert_eq!(normalize_shorthands("due=..+1d"), "due=..1d");
        assert_eq!(normalize_shorthands("due=+2d.."), "due=2d..");
        assert_eq!(normalize_shorthands("due=today..+5d"), "due=today..5d");
    }

    #[test]
    fn normalize_compound_group() {
        assert_eq!(normalize_shorthands("@joint;due=..+1d"), "@=joint;due=..1d");
    }

    #[test]
    fn normalize_passthrough_already_eq() {
        // Conditions that already have '=' pass through (only date offsets stripped).
        assert_eq!(normalize_shorthands("pri=any"), "pri=any");
        assert_eq!(normalize_shorthands("due=any"), "due=any");
        assert_eq!(normalize_shorthands("done=none"), "done=none");
    }

    // ── context matching ─────────────────────────────────────────────────────

    #[test]
    fn context_exact_match() {
        let f = Filter::parse("@today");
        assert!(f.matches(&task_ctx("today"), today()));
        assert!(!f.matches(&Task::default(), today()));
        assert!(!f.matches(&task_ctx("work"), today()));
    }

    #[test]
    fn context_exclude() {
        let f = Filter::parse("-@work");
        assert!(f.matches(&Task::default(), today()));
        assert!(f.matches(&task_ctx("home"), today()));
        assert!(!f.matches(&task_ctx("work"), today()));
    }

    #[test]
    fn context_any_and_none() {
        let any = Filter::parse("@=any");
        let none = Filter::parse("@=none");
        assert!(!any.matches(&Task::default(), today()));
        assert!(any.matches(&task_ctx("foo"), today()));
        assert!(none.matches(&Task::default(), today()));
        assert!(!none.matches(&task_ctx("foo"), today()));
    }

    #[test]
    fn context_wildcard_prefix() {
        let f = Filter::parse("@=work*");
        assert!(f.matches(&task_ctx("work"), today()));
        assert!(f.matches(&task_ctx("workout"), today()));
        assert!(!f.matches(&task_ctx("atwork"), today()));
    }

    // ── priority matching ─────────────────────────────────────────────────────

    #[test]
    fn priority_any() {
        let f = Filter::parse("pri=any");
        assert!(f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_pri('z'), today()));
        assert!(!f.matches(&Task::default(), today()));
    }

    #[test]
    fn priority_none() {
        let f = Filter::parse("pri=none");
        assert!(f.matches(&Task::default(), today()));
        assert!(!f.matches(&task_pri('a'), today()));
    }

    #[test]
    fn priority_exact() {
        let f = Filter::parse("pri=B");
        assert!(!f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_pri('b'), today()));
        assert!(!f.matches(&task_pri('c'), today()));
    }

    #[test]
    fn priority_range() {
        let f = Filter::parse("pri=B..D");
        assert!(!f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_pri('b'), today()));
        assert!(f.matches(&task_pri('c'), today()));
        assert!(f.matches(&task_pri('d'), today()));
        assert!(!f.matches(&task_pri('e'), today()));
    }

    // ── due date matching ─────────────────────────────────────────────────────

    #[test]
    fn due_any() {
        let f = Filter::parse("due=any");
        assert!(!f.matches(&Task::default(), today()));
        assert!(f.matches(&task_due(0), today()));
        assert!(f.matches(&task_due(10), today()));
    }

    #[test]
    fn due_none() {
        let f = Filter::parse("due=none");
        assert!(f.matches(&Task::default(), today()));
        assert!(!f.matches(&task_due(0), today()));
    }

    #[test]
    fn due_up_to_2d() {
        // +2d is normalised to 2d
        let f = Filter::parse("due=..+2d");
        assert!(f.matches(&task_due(-5), today())); // overdue
        assert!(f.matches(&task_due(0), today())); // today
        assert!(f.matches(&task_due(2), today())); // 2 days out
        assert!(!f.matches(&task_due(3), today())); // 3 days out
        assert!(!f.matches(&Task::default(), today())); // no due date
    }

    #[test]
    fn due_from() {
        let f = Filter::parse("due=+7d..");
        assert!(!f.matches(&task_due(6), today()));
        assert!(f.matches(&task_due(7), today()));
        assert!(f.matches(&task_due(100), today()));
        assert!(!f.matches(&Task::default(), today()));
    }

    #[test]
    fn due_on_today() {
        let f = Filter::parse("due=today");
        assert!(f.matches(&task_due(0), today()));
        assert!(!f.matches(&task_due(1), today()));
        assert!(!f.matches(&task_due(-1), today()));
    }

    // ── done matching ─────────────────────────────────────────────────────────

    #[test]
    fn done_none() {
        let f = Filter::parse("done=none");
        let done = task_from("x 2026-01-01 2026-01-01 finished task");
        assert!(f.matches(&Task::default(), today()));
        assert!(!f.matches(&done, today()));
    }

    #[test]
    fn done_any() {
        let f = Filter::parse("done=any");
        let done = task_from("x 2026-01-01 2026-01-01 finished task");
        assert!(!f.matches(&Task::default(), today()));
        assert!(f.matches(&done, today()));
    }

    // ── composite ────────────────────────────────────────────────────────────

    #[test]
    fn or_rule_any_match_wins() {
        let f = Filter::parse("@today~pri=any~due=any");
        assert!(f.matches(&task_ctx("today"), today()));
        assert!(f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_due(5), today()));
        assert!(!f.matches(&Task::default(), today()));
    }

    #[test]
    fn and_rule_all_must_match() {
        let f = Filter::parse("@joint;due=..+2d");
        // both conditions met
        let t = task_from(&format!(
            "task @joint due:{}",
            (today() + Duration::days(1)).format("%Y-%m-%d")
        ));
        assert!(f.matches(&t, today()));
        // missing due date
        assert!(!f.matches(&task_ctx("joint"), today()));
        // missing context
        assert!(!f.matches(&task_due(1), today()));
    }

    #[test]
    fn deny_all_never_matches() {
        let f = Filter::deny_all();
        assert!(!f.matches(&Task::default(), today()));
        assert!(!f.matches(&task_ctx("today"), today()));
    }

    #[test]
    fn pipe_or_separator_works() {
        let f = Filter::parse("@today|pri=any");
        assert!(f.matches(&task_ctx("today"), today()));
        assert!(f.matches(&task_pri('b'), today()));
    }

    #[test]
    fn unknown_field_produces_deny_all() {
        // Unknown fields parse without error but match nothing (flt passthrough behaviour)
        let f = Filter::parse("bogus=value");
        assert!(!f.matches(&Task::default(), today()));
    }
}
