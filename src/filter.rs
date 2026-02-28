//! TTDL-compatible push-filter expression parser and evaluator.
//!
//! Syntax (subset of TTDL `--filter`):
//!
//! ```text
//! filter = rule ('~' rule)*
//! rule   = cond (';' cond)*
//! cond   = '@'text           -- include context
//!        | '-@'text          -- exclude context
//!        | '+'text           -- include project
//!        | '-+'text          -- exclude project
//!        | field '=' value
//! field  = 'context' | 'ctx' | 'project' | 'prj'
//!        | 'priority' | 'pri' | 'due' | 'done'
//! value  = 'any' | 'none' | text | pri-spec | date-spec
//! ```
//!
//! Rules are OR-combined; conditions within a rule are AND-combined.
//! Examples:
//!   `@joint;due=..+2d`       — @joint context AND due within 2 days
//!   `@today~pri=any~due=any` — @today OR any priority OR any due date

use chrono::NaiveDate;
use todo_lib::todotxt::Task;

const NO_PRIORITY: u8 = todo_lib::todotxt::NO_PRIORITY;

/// A parsed push-filter expression.
///
/// Rules are OR-combined; conditions within a rule are AND-combined.
#[derive(Debug, Clone)]
pub struct Filter {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    conditions: Vec<Condition>,
}

#[derive(Debug, Clone)]
enum Condition {
    Context { pat: StrPat, include: bool },
    Project { pat: StrPat, include: bool },
    Priority(PriSpec),
    Due(DueSpec),
    Done(bool),
}

/// Pattern match for string fields (contexts, projects).
#[derive(Debug, Clone)]
enum StrPat {
    Any,
    None,
    Prefix(String),
    Suffix(String),
    Contains(String),
    Exact(String),
}

/// Priority filter specification.
///
/// In todo_lib, priority is stored as a `u8` where A=0, B=1, ..., Z=25,
/// and `NO_PRIORITY`=26 means no priority set.
/// "Higher" priority means a lower `u8` value.
#[derive(Debug, Clone)]
enum PriSpec {
    /// Any priority set (`pri=any`)
    Any,
    /// No priority (`pri=none`)
    None,
    /// Exact letter (`pri=b` → u8 = 1)
    Exact(u8),
    /// This priority or higher/more-important (`pri=b+` → u8 ≤ 1, i.e. A or B)
    AtLeast(u8),
    /// Inclusive range (`pri=a..c` → 0 ≤ u8 ≤ 2)
    Range(u8, u8),
}

/// Due-date filter specification.
///
/// Day offsets are relative to today (0 = today, 1 = tomorrow, -1 = yesterday).
/// All variants except `Any` and `None` require the task to *have* a due date.
#[derive(Debug, Clone)]
enum DueSpec {
    /// Has any due date (`due=any`)
    Any,
    /// Has no due date (`due=none`)
    None,
    /// Due-diff ≤ n (`due=..+2d`)
    UpTo(i64),
    /// Due-diff ≥ n (`due=+2d..`)
    From(i64),
    /// n1 ≤ due-diff ≤ n2 (`due=today..+5d`)
    Between(i64, i64),
    /// Due-diff == n (`due=today`, `due=+1d`)
    On(i64),
}

// ============================================================
// Public API
// ============================================================

impl Filter {
    /// Parse a filter expression string. Returns an error message on failure.
    pub fn parse(s: &str) -> Result<Self, String> {
        let rules = s
            .split(['~', '|'])
            .map(|r| Rule::parse(r.trim()))
            .collect::<Result<Vec<_>, _>>()?;
        if rules.is_empty() {
            return Err("empty filter expression".to_string());
        }
        Ok(Filter { rules })
    }

    /// Returns a filter that never matches anything (used on parse error).
    pub fn deny_all() -> Self {
        Filter { rules: vec![] }
    }

    /// Returns `true` if the task matches this filter, evaluated against `today`.
    pub fn matches(&self, task: &Task, today: NaiveDate) -> bool {
        self.rules.iter().any(|r| r.matches(task, today))
    }
}

// ============================================================
// Rule
// ============================================================

impl Rule {
    fn parse(s: &str) -> Result<Self, String> {
        let conditions = s
            .split(';')
            .map(|c| Condition::parse(c.trim()))
            .collect::<Result<Vec<_>, _>>()?;
        if conditions.is_empty() {
            return Err("empty rule".to_string());
        }
        Ok(Rule { conditions })
    }

    fn matches(&self, task: &Task, today: NaiveDate) -> bool {
        self.conditions.iter().all(|c| c.matches(task, today))
    }
}

// ============================================================
// Condition
// ============================================================

impl Condition {
    fn parse(s: &str) -> Result<Self, String> {
        // @context  or  -@context
        if let Some(rest) = s.strip_prefix("-@") {
            return Ok(Condition::Context {
                pat: StrPat::parse(rest),
                include: false,
            });
        }
        if let Some(rest) = s.strip_prefix('@') {
            return Ok(Condition::Context {
                pat: StrPat::parse(rest),
                include: true,
            });
        }
        // +project  or  -+project
        if let Some(rest) = s.strip_prefix("-+") {
            return Ok(Condition::Project {
                pat: StrPat::parse(rest),
                include: false,
            });
        }
        if let Some(rest) = s.strip_prefix('+') {
            return Ok(Condition::Project {
                pat: StrPat::parse(rest),
                include: true,
            });
        }
        // field=value
        let (field, value) = s
            .split_once('=')
            .ok_or_else(|| format!("invalid condition (no '='): {s:?}"))?;
        let field = field.trim().to_lowercase();
        let value = value.trim();

        match field.as_str() {
            "context" | "ctx" => {
                let (include, val) = if let Some(v) = value.strip_prefix('-') {
                    (false, v)
                } else {
                    (true, value)
                };
                Ok(Condition::Context {
                    pat: StrPat::parse(val),
                    include,
                })
            }
            "project" | "prj" => {
                let (include, val) = if let Some(v) = value.strip_prefix('-') {
                    (false, v)
                } else {
                    (true, value)
                };
                Ok(Condition::Project {
                    pat: StrPat::parse(val),
                    include,
                })
            }
            "priority" | "pri" => Ok(Condition::Priority(PriSpec::parse(value)?)),
            "due" => Ok(Condition::Due(DueSpec::parse(value)?)),
            "done" => match value.to_lowercase().as_str() {
                "true" | "yes" | "1" => Ok(Condition::Done(true)),
                "false" | "no" | "0" => Ok(Condition::Done(false)),
                _ => Err(format!("invalid done value: {value:?} (use true/false)")),
            },
            other => Err(format!("unknown filter field: {other:?}")),
        }
    }

    fn matches(&self, task: &Task, today: NaiveDate) -> bool {
        match self {
            Condition::Context { pat, include: true } => match pat {
                StrPat::Any => !task.contexts.is_empty(),
                StrPat::None => task.contexts.is_empty(),
                pat => task.contexts.iter().any(|c| pat.matches_str(c)),
            },
            Condition::Context {
                pat,
                include: false,
            } => match pat {
                StrPat::Any => task.contexts.is_empty(),
                StrPat::None => !task.contexts.is_empty(),
                pat => !task.contexts.iter().any(|c| pat.matches_str(c)),
            },
            Condition::Project { pat, include: true } => match pat {
                StrPat::Any => !task.projects.is_empty(),
                StrPat::None => task.projects.is_empty(),
                pat => task.projects.iter().any(|p| pat.matches_str(p)),
            },
            Condition::Project {
                pat,
                include: false,
            } => match pat {
                StrPat::Any => task.projects.is_empty(),
                StrPat::None => !task.projects.is_empty(),
                pat => !task.projects.iter().any(|p| pat.matches_str(p)),
            },
            Condition::Priority(spec) => spec.matches(task.priority),
            Condition::Due(spec) => spec.matches(task.due_date, today),
            Condition::Done(expected) => task.finished == *expected,
        }
    }
}

// ============================================================
// StrPat
// ============================================================

impl StrPat {
    fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "any" => StrPat::Any,
            "none" => StrPat::None,
            _ => {
                let star_prefix = s.starts_with('*');
                let star_suffix = s.ends_with('*');
                let inner = match (star_prefix, star_suffix) {
                    (true, true) => &s[1..s.len() - 1],
                    (true, false) => &s[1..],
                    (false, true) => &s[..s.len() - 1],
                    (false, false) => s,
                };
                let inner = inner.to_lowercase();
                match (star_prefix, star_suffix) {
                    (true, true) => StrPat::Contains(inner),
                    (true, false) => StrPat::Suffix(inner),
                    (false, true) => StrPat::Prefix(inner),
                    (false, false) => StrPat::Exact(inner),
                }
            }
        }
    }

    fn matches_str(&self, s: &str) -> bool {
        let low = s.to_lowercase();
        match self {
            StrPat::Any => true,
            StrPat::None => false,
            StrPat::Exact(v) => &low == v,
            StrPat::Prefix(v) => low.starts_with(v.as_str()),
            StrPat::Suffix(v) => low.ends_with(v.as_str()),
            StrPat::Contains(v) => low.contains(v.as_str()),
        }
    }
}

// ============================================================
// PriSpec
// ============================================================

impl PriSpec {
    fn parse(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "any" | "+" => return Ok(PriSpec::Any),
            "none" | "-" => return Ok(PriSpec::None),
            _ => {}
        }
        // Range: "a..c"
        if let Some((lo, hi)) = s.split_once("..") {
            return Ok(PriSpec::Range(parse_pri_char(lo)?, parse_pri_char(hi)?));
        }
        // "b+" = B or higher (more important) priority
        if let Some(base) = s.strip_suffix('+') {
            return Ok(PriSpec::AtLeast(parse_pri_char(base)?));
        }
        Ok(PriSpec::Exact(parse_pri_char(s)?))
    }

    fn matches(&self, priority: u8) -> bool {
        match self {
            PriSpec::Any => priority < NO_PRIORITY,
            PriSpec::None => priority == NO_PRIORITY,
            PriSpec::Exact(v) => priority == *v,
            // "B or higher" = u8 ≤ 1 (A=0, B=1; lower u8 = higher priority)
            PriSpec::AtLeast(v) => priority < NO_PRIORITY && priority <= *v,
            PriSpec::Range(lo, hi) => priority < NO_PRIORITY && priority >= *lo && priority <= *hi,
        }
    }
}

fn parse_pri_char(s: &str) -> Result<u8, String> {
    let s = s.trim();
    if s.len() == 1 {
        let c = s.chars().next().unwrap().to_ascii_lowercase();
        if c.is_ascii_alphabetic() {
            return Ok(c as u8 - b'a');
        }
    }
    Err(format!("invalid priority letter: {s:?}"))
}

// ============================================================
// DueSpec
// ============================================================

impl DueSpec {
    fn parse(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "any" | "+" => return Ok(DueSpec::Any),
            "none" | "-" => return Ok(DueSpec::None),
            _ => {}
        }
        // "..value" = up to (UpTo)
        if let Some(end_str) = s.strip_prefix("..") {
            return Ok(DueSpec::UpTo(parse_date_offset(end_str)?));
        }
        // "value.." = from (From)
        if let Some(start_str) = s.strip_suffix("..") {
            return Ok(DueSpec::From(parse_date_offset(start_str)?));
        }
        // "start..end" = between (Between)
        if let Some((start_str, end_str)) = s.split_once("..") {
            return Ok(DueSpec::Between(
                parse_date_offset(start_str)?,
                parse_date_offset(end_str)?,
            ));
        }
        // Single date = On
        Ok(DueSpec::On(parse_date_offset(s)?))
    }

    fn matches(&self, due_date: Option<NaiveDate>, today: NaiveDate) -> bool {
        match self {
            DueSpec::Any => due_date.is_some(),
            DueSpec::None => due_date.is_none(),
            DueSpec::UpTo(n) => due_date
                .map(|d| (d - today).num_days() <= *n)
                .unwrap_or(false),
            DueSpec::From(n) => due_date
                .map(|d| (d - today).num_days() >= *n)
                .unwrap_or(false),
            DueSpec::Between(lo, hi) => due_date
                .map(|d| {
                    let diff = (d - today).num_days();
                    diff >= *lo && diff <= *hi
                })
                .unwrap_or(false),
            DueSpec::On(n) => due_date
                .map(|d| (d - today).num_days() == *n)
                .unwrap_or(false),
        }
    }
}

/// Parse a relative date expression to a day offset from today.
///
/// Supports: `today` (0), `yesterday` (-1), `tomorrow` (+1),
/// `+Nd` / `-Nd` (days), `+Nw` / `-Nw` (weeks), `+Nm` / `-Nm` (months ≈ 30d).
fn parse_date_offset(s: &str) -> Result<i64, String> {
    match s.to_lowercase().as_str() {
        "today" => return Ok(0),
        "yesterday" => return Ok(-1),
        "tomorrow" => return Ok(1),
        _ => {}
    }

    let (sign, rest) = if let Some(r) = s.strip_prefix('+') {
        (1i64, r)
    } else if let Some(r) = s.strip_prefix('-') {
        (-1i64, r)
    } else {
        return Err(format!(
            "invalid date offset: {s:?} (expected today/yesterday/tomorrow/+Nd/-Nd)"
        ));
    };

    if let Some(n_str) = rest.strip_suffix('d') {
        let n: i64 = n_str
            .parse()
            .map_err(|_| format!("invalid date offset: {s:?}"))?;
        return Ok(sign * n);
    }
    if let Some(n_str) = rest.strip_suffix('w') {
        let n: i64 = n_str
            .parse()
            .map_err(|_| format!("invalid date offset: {s:?}"))?;
        return Ok(sign * n * 7);
    }
    if let Some(n_str) = rest.strip_suffix('m') {
        let n: i64 = n_str
            .parse()
            .map_err(|_| format!("invalid date offset: {s:?}"))?;
        return Ok(sign * n * 30);
    }
    Err(format!(
        "invalid date offset: {s:?} (expected +Nd, +Nw, or +Nm)"
    ))
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

    fn task_ctx(ctx: &str) -> Task {
        let mut t = Task::default();
        t.contexts.push(ctx.to_string());
        t
    }

    fn task_pri(p: char) -> Task {
        let mut t = Task::default();
        t.priority = p.to_ascii_lowercase() as u8 - b'a';
        t
    }

    fn task_due(days: i64) -> Task {
        let mut t = Task::default();
        t.due_date = Some(today() + Duration::days(days));
        t
    }

    // ── parsing ──────────────────────────────────────────────────────────────

    #[test]
    fn parse_context_shorthand() {
        assert!(Filter::parse("@today").is_ok());
        assert!(Filter::parse("-@work").is_ok());
    }

    #[test]
    fn parse_field_eq_value() {
        assert!(Filter::parse("context=today").is_ok());
        assert!(Filter::parse("pri=any").is_ok());
        assert!(Filter::parse("due=..+2d").is_ok());
        assert!(Filter::parse("done=false").is_ok());
    }

    #[test]
    fn parse_or_and_combined() {
        assert!(Filter::parse("@today~pri=any~due=any").is_ok());
        assert!(Filter::parse("@joint;due=..+2d").is_ok());
    }

    #[test]
    fn parse_error_unknown_field() {
        assert!(Filter::parse("bogus=value").is_err());
    }

    // ── context matching ─────────────────────────────────────────────────────

    #[test]
    fn context_exact_match() {
        let f = Filter::parse("@today").unwrap();
        assert!(f.matches(&task_ctx("today"), today()));
        assert!(!f.matches(&Task::default(), today()));
        assert!(!f.matches(&task_ctx("work"), today()));
    }

    #[test]
    fn context_exclude() {
        let f = Filter::parse("-@work").unwrap();
        assert!(f.matches(&Task::default(), today()));
        assert!(f.matches(&task_ctx("home"), today()));
        assert!(!f.matches(&task_ctx("work"), today()));
    }

    #[test]
    fn context_any_and_none() {
        let any = Filter::parse("@any").unwrap();
        let none = Filter::parse("@none").unwrap();
        assert!(!any.matches(&Task::default(), today()));
        assert!(any.matches(&task_ctx("foo"), today()));
        assert!(none.matches(&Task::default(), today()));
        assert!(!none.matches(&task_ctx("foo"), today()));
    }

    #[test]
    fn context_wildcard_prefix() {
        let f = Filter::parse("@work*").unwrap();
        assert!(f.matches(&task_ctx("work"), today()));
        assert!(f.matches(&task_ctx("workout"), today()));
        assert!(!f.matches(&task_ctx("atwork"), today()));
    }

    // ── priority matching ─────────────────────────────────────────────────────

    #[test]
    fn priority_any() {
        let f = Filter::parse("pri=any").unwrap();
        assert!(f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_pri('z'), today()));
        assert!(!f.matches(&Task::default(), today()));
    }

    #[test]
    fn priority_none() {
        let f = Filter::parse("pri=none").unwrap();
        assert!(f.matches(&Task::default(), today()));
        assert!(!f.matches(&task_pri('a'), today()));
    }

    #[test]
    fn priority_exact() {
        let f = Filter::parse("pri=b").unwrap();
        assert!(!f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_pri('b'), today()));
        assert!(!f.matches(&task_pri('c'), today()));
    }

    #[test]
    fn priority_at_least_b_plus() {
        // pri=b+ means B or higher (more important), i.e. A or B
        let f = Filter::parse("pri=b+").unwrap();
        assert!(f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_pri('b'), today()));
        assert!(!f.matches(&task_pri('c'), today()));
        assert!(!f.matches(&Task::default(), today()));
    }

    #[test]
    fn priority_range() {
        let f = Filter::parse("pri=b..d").unwrap();
        assert!(!f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_pri('b'), today()));
        assert!(f.matches(&task_pri('c'), today()));
        assert!(f.matches(&task_pri('d'), today()));
        assert!(!f.matches(&task_pri('e'), today()));
    }

    // ── due date matching ─────────────────────────────────────────────────────

    #[test]
    fn due_any() {
        let f = Filter::parse("due=any").unwrap();
        assert!(!f.matches(&Task::default(), today()));
        assert!(f.matches(&task_due(0), today()));
        assert!(f.matches(&task_due(10), today()));
    }

    #[test]
    fn due_none() {
        let f = Filter::parse("due=none").unwrap();
        assert!(f.matches(&Task::default(), today()));
        assert!(!f.matches(&task_due(0), today()));
    }

    #[test]
    fn due_up_to_2d() {
        let f = Filter::parse("due=..+2d").unwrap();
        assert!(f.matches(&task_due(-5), today())); // overdue
        assert!(f.matches(&task_due(0), today())); // today
        assert!(f.matches(&task_due(2), today())); // day after tomorrow
        assert!(!f.matches(&task_due(3), today())); // 3 days out
        assert!(!f.matches(&Task::default(), today())); // no due date
    }

    #[test]
    fn due_from() {
        let f = Filter::parse("due=+7d..").unwrap();
        assert!(!f.matches(&task_due(6), today()));
        assert!(f.matches(&task_due(7), today()));
        assert!(f.matches(&task_due(100), today()));
        assert!(!f.matches(&Task::default(), today()));
    }

    #[test]
    fn due_on_today() {
        let f = Filter::parse("due=today").unwrap();
        assert!(f.matches(&task_due(0), today()));
        assert!(!f.matches(&task_due(1), today()));
        assert!(!f.matches(&task_due(-1), today()));
    }

    // ── done matching ─────────────────────────────────────────────────────────

    #[test]
    fn done_false() {
        let f = Filter::parse("done=false").unwrap();
        let mut done = Task::default();
        done.finished = true;
        assert!(f.matches(&Task::default(), today()));
        assert!(!f.matches(&done, today()));
    }

    // ── composite ────────────────────────────────────────────────────────────

    #[test]
    fn or_rule_any_match_wins() {
        let f = Filter::parse("@today~pri=any~due=any").unwrap();
        assert!(f.matches(&task_ctx("today"), today()));
        assert!(f.matches(&task_pri('a'), today()));
        assert!(f.matches(&task_due(5), today()));
        assert!(!f.matches(&Task::default(), today()));
    }

    #[test]
    fn and_rule_all_must_match() {
        let f = Filter::parse("@joint;due=..+2d").unwrap();
        // both conditions met
        let mut t = task_ctx("joint");
        t.due_date = Some(today() + Duration::days(1));
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
}
