use serde::{Deserialize, Serialize};

/// A reminder as returned by the Swift CLI (camelCase JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Fields used for deserialization and future sync phases
pub struct Reminder {
    pub id: String,
    pub external_id: String,
    pub title: String,
    pub due_date: Option<String>,
    pub priority: i32,
    pub is_completed: bool,
    pub completion_date: Option<String>,
    pub creation_date: Option<String>,
    pub last_modified_date: Option<String>,
    pub notes: Option<String>,
    pub list: String,
}

/// A reminder list as returned by the Swift CLI.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // Used by list_lists command
pub struct ReminderList {
    pub id: String,
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_full_reminder() {
        let json = r#"{
            "id": "abc-123",
            "externalId": "ext-456",
            "title": "Buy milk",
            "dueDate": "2026-03-01",
            "priority": 0,
            "isCompleted": false,
            "completionDate": null,
            "creationDate": "2026-02-20",
            "lastModifiedDate": "2026-02-20T10:00:00Z",
            "notes": "Call back dentist",
            "list": "Tasks"
        }"#;
        let r: Reminder = serde_json::from_str(json).unwrap();
        assert_eq!(r.id, "abc-123");
        assert_eq!(r.external_id, "ext-456");
        assert_eq!(r.title, "Buy milk");
        assert_eq!(r.due_date, Some("2026-03-01".to_string()));
        assert_eq!(r.priority, 0);
        assert!(!r.is_completed);
        assert!(r.completion_date.is_none());
        assert_eq!(r.creation_date, Some("2026-02-20".to_string()));
        assert_eq!(
            r.last_modified_date,
            Some("2026-02-20T10:00:00Z".to_string())
        );
        assert_eq!(r.notes, Some("Call back dentist".to_string()));
        assert_eq!(r.list, "Tasks");
    }

    #[test]
    fn test_deserialize_minimal_reminder() {
        let json = r#"{
            "id": "abc-123",
            "externalId": "ext-456",
            "title": "Quick task",
            "dueDate": null,
            "priority": 0,
            "isCompleted": false,
            "completionDate": null,
            "creationDate": null,
            "lastModifiedDate": null,
            "notes": null,
            "list": "Tasks"
        }"#;
        let r: Reminder = serde_json::from_str(json).unwrap();
        assert_eq!(r.title, "Quick task");
        assert!(r.due_date.is_none());
        assert!(r.creation_date.is_none());
        assert!(r.notes.is_none());
    }

    #[test]
    fn test_deserialize_completed_reminder() {
        let json = r#"{
            "id": "abc-123",
            "externalId": "ext-456",
            "title": "Done task",
            "dueDate": "2026-02-28",
            "priority": 9,
            "isCompleted": true,
            "completionDate": "2026-02-25",
            "creationDate": "2026-02-20",
            "lastModifiedDate": "2026-02-25T14:30:00Z",
            "notes": null,
            "list": "Shopping"
        }"#;
        let r: Reminder = serde_json::from_str(json).unwrap();
        assert!(r.is_completed);
        assert_eq!(r.completion_date, Some("2026-02-25".to_string()));
        assert_eq!(r.priority, 9);
        assert_eq!(r.list, "Shopping");
    }

    #[test]
    fn test_deserialize_reminder_list() {
        let json = r#"[
            {"id": "list-1", "title": "Tasks"},
            {"id": "list-2", "title": "Shopping"}
        ]"#;
        let lists: Vec<ReminderList> = serde_json::from_str(json).unwrap();
        assert_eq!(lists.len(), 2);
        assert_eq!(lists[0].title, "Tasks");
        assert_eq!(lists[1].id, "list-2");
    }
}
