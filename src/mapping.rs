use chrono::Local;
use todo_lib::todotxt::Task;

use crate::reminder::Reminder;
use crate::sync::config::{MappingTarget, PriorityMap};

/// Convert a Reminder to a todo.txt Task using the given priority map.
///
/// Builds a todo.txt line string and parses it via `Task::parse()` to ensure
/// all fields (subject, contexts, projects, tags) stay consistent.
///
/// Mapping applied to incomplete tasks only:
/// - `MappingTarget::Priority(p)` → letter priority prefix, e.g. `(A)`
/// - `MappingTarget::Context(ctx)` → `@ctx` appended after the subject
/// - `MappingTarget::Nothing` → no priority representation
///
/// Other fields:
/// - `dueDateComponents` → `due:YYYY-MM-DD`
/// - `isCompleted` → `x` prefix with completion/creation dates
/// - `calendarItemExternalIdentifier` → `eid:value`
///
/// Notes are intentionally NOT written to todo.txt — they live in Reminders
/// only (and are stored in state.json for diff purposes).
pub fn reminder_to_task(r: &Reminder, map: &PriorityMap) -> Task {
    let mut parts = Vec::new();

    // Compute the priority representation up front (only for incomplete tasks).
    let priority_repr = if !r.is_completed {
        map.reminders_to_task(r.priority).clone()
    } else {
        MappingTarget::Nothing
    };

    if r.is_completed {
        parts.push("x".to_string());
        if let Some(ref date) = r.completion_date {
            parts.push(date.clone());
        }
        if let Some(ref date) = r.creation_date {
            parts.push(date.clone());
        }
    } else {
        // Letter priority must come BEFORE the creation date in todo.txt format.
        if let MappingTarget::Priority(p) = &priority_repr {
            parts.push(format!("({})", (b'A' + p) as char));
        }
        if let Some(ref date) = r.creation_date {
            parts.push(date.clone());
        }
    }

    // Title (subject)
    parts.push(r.title.clone());

    // Context mapping: appended after subject (incomplete tasks only).
    if let MappingTarget::Context(ctx) = &priority_repr {
        parts.push(format!("@{ctx}"));
    }

    // due date
    if let Some(ref date) = r.due_date {
        parts.push(format!("due:{date}"));
    }

    // eid (sync identity)
    parts.push(format!("eid:{}", r.external_id));

    let line = parts.join(" ");
    let base = Local::now().date_naive();
    Task::parse(&line, base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_reminder() -> Reminder {
        Reminder {
            id: "abc-123".to_string(),
            external_id: "ext-456".to_string(),
            title: "Buy milk".to_string(),
            due_date: Some("2026-03-01".to_string()),
            priority: 0,
            is_completed: false,
            completion_date: None,
            creation_date: Some("2026-02-20".to_string()),
            last_modified_date: None,
            notes: None,
            list: "Tasks".to_string(),
        }
    }

    fn default_map() -> PriorityMap {
        PriorityMap::default()
    }

    #[test]
    fn test_basic_task() {
        let task = reminder_to_task(&base_reminder(), &default_map());
        let line = format!("{task}");
        assert!(line.contains("Buy milk"));
        assert!(!line.contains("+tasks"));
        assert!(line.contains("due:2026-03-01"));
        assert!(line.contains("eid:ext-456"));
        assert!(line.starts_with("2026-02-20 "));
        assert!(!line.contains("@today"));
    }

    #[test]
    fn test_completed_task() {
        let mut r = base_reminder();
        r.is_completed = true;
        r.completion_date = Some("2026-02-24".to_string());

        let task = reminder_to_task(&r, &default_map());
        let line = format!("{task}");
        assert!(line.starts_with("x 2026-02-24 2026-02-20 "));
        assert!(line.contains("Buy milk"));
        assert!(line.contains("due:2026-03-01"));
        assert!(!line.contains("@today"));
    }

    #[test]
    fn test_priority_9_adds_today() {
        let mut r = base_reminder();
        r.priority = 9;

        let task = reminder_to_task(&r, &default_map());
        let line = format!("{task}");
        assert!(line.contains("@today"));
        assert!(task.contexts.contains(&"today".to_string()));
    }

    #[test]
    fn test_priority_0_no_today() {
        let task = reminder_to_task(&base_reminder(), &default_map());
        let line = format!("{task}");
        assert!(!line.contains("@today"));
    }

    #[test]
    fn test_priority_1_no_letter_priority_default_map() {
        let mut r = base_reminder();
        r.priority = 1;

        let task = reminder_to_task(&r, &default_map());
        let line = format!("{task}");
        // Default map: only priority 9 is mapped; priority 1 has no representation.
        assert!(!line.starts_with("(A)"));
        assert!(!line.starts_with("(B)"));
        assert!(!line.starts_with("(C)"));
    }

    #[test]
    fn test_custom_map_priority_1_to_letter_a() {
        let mut r = base_reminder();
        r.priority = 1;

        let map = PriorityMap::from_config(&{
            let mut m = std::collections::HashMap::new();
            m.insert("1".to_string(), "priority:A".to_string());
            m.insert("5".to_string(), "priority:B".to_string());
            m.insert("9".to_string(), "context:today".to_string());
            m
        });

        let task = reminder_to_task(&r, &map);
        let line = format!("{task}");
        assert!(line.contains("(A)"), "expected (A) in: {line}");
        assert_eq!(task.priority, 0); // A = 0
    }

    #[test]
    fn test_custom_map_priority_9_to_custom_context() {
        let mut r = base_reminder();
        r.priority = 9;

        let map = PriorityMap::from_config(&{
            let mut m = std::collections::HashMap::new();
            m.insert("9".to_string(), "context:urgent".to_string());
            m
        });

        let task = reminder_to_task(&r, &map);
        let line = format!("{task}");
        assert!(line.contains("@urgent"), "expected @urgent in: {line}");
        assert!(!line.contains("@today"));
    }

    #[test]
    fn test_completed_priority_9_no_today() {
        let mut r = base_reminder();
        r.priority = 9;
        r.is_completed = true;
        r.completion_date = Some("2026-02-25".to_string());

        let task = reminder_to_task(&r, &default_map());
        let line = format!("{task}");
        // @today should NOT appear for completed tasks
        assert!(!line.contains("@today"));
    }

    #[test]
    fn test_no_project_tag() {
        let mut r = base_reminder();
        r.list = "My Shopping".to_string();

        let task = reminder_to_task(&r, &default_map());
        let line = format!("{task}");
        assert!(!line.contains("+my_shopping"));
        assert!(!line.contains("+tasks"));
        assert!(task.projects.is_empty());
    }

    #[test]
    fn test_no_due_date() {
        let mut r = base_reminder();
        r.due_date = None;

        let task = reminder_to_task(&r, &default_map());
        let line = format!("{task}");
        assert!(!line.contains("due:"));
    }

    #[test]
    fn test_roundtrip() {
        let task = reminder_to_task(&base_reminder(), &default_map());
        let line = format!("{task}");
        let reparsed = Task::parse(&line, Local::now().date_naive());
        assert_eq!(format!("{reparsed}"), line);
    }
}
