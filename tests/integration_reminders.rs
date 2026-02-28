//! Integration tests for the Swift CLI write-back commands.
//!
//! These tests exercise the real Apple Reminders database through the Swift CLI.
//! All tests are `#[ignore]` so they are skipped by normal `cargo test` and pre-commit.
//!
//! Run with:
//! ```
//! cargo test --test integration_reminders -- --ignored --test-threads=1
//! ```
//!
//! Prerequisites:
//! - macOS with Reminders access granted for the binary
//! - Swift CLI built: `cd swift && swift build -c release`

use remtodo::reminder::Reminder;
use remtodo::swift_cli::{CreateReminderInput, SwiftCli};
use remtodo::sync::actions::ReminderUpdate;

const TEST_LIST: &str = "remtodo-integration-test";

// ---------------------------------------------------------------------------
// RAII cleanup guard
// ---------------------------------------------------------------------------

struct TestGuard {
    cli: SwiftCli,
    tracked_eids: Vec<String>,
    list_created_by_test: bool,
}

impl TestGuard {
    /// Initialise the guard: create the test list if it doesn't already exist.
    fn setup() -> Self {
        let cli = SwiftCli::new().expect("SwiftCli::new() failed — is reminders-helper built?");

        let existing = cli
            .list_lists()
            .expect("list_lists() failed")
            .into_iter()
            .any(|l| l.title == TEST_LIST);

        let list_created_by_test = if !existing {
            cli.create_list(TEST_LIST)
                .unwrap_or_else(|e| panic!("create_list({TEST_LIST}) failed: {e}"));
            true
        } else {
            false
        };

        TestGuard {
            cli,
            tracked_eids: Vec::new(),
            list_created_by_test,
        }
    }

    /// Create a reminder and track its eid for cleanup.
    fn create(&mut self, input: CreateReminderInput) -> Reminder {
        let reminder = self
            .cli
            .create_reminder(&input)
            .unwrap_or_else(|e| panic!("create_reminder failed: {e}"));
        self.tracked_eids.push(reminder.external_id.clone());
        reminder
    }

    /// Fetch all reminders from the test list (including completed).
    fn get_all(&self) -> Vec<Reminder> {
        self.cli
            .get_reminders(TEST_LIST, true)
            .unwrap_or_else(|e| panic!("get_reminders failed: {e}"))
    }

    /// Find a reminder in the test list by eid.
    fn find_by_eid(&self, eid: &str) -> Option<Reminder> {
        self.get_all().into_iter().find(|r| r.external_id == eid)
    }
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        // Best-effort cleanup of tracked reminders.
        for eid in &self.tracked_eids {
            let _ = self.cli.delete_reminder(eid, TEST_LIST);
        }
        // Only delete the list if this test created it.
        if self.list_created_by_test {
            let _ = self.cli.delete_list(TEST_LIST);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper to build a minimal CreateReminderInput
// ---------------------------------------------------------------------------

fn make_input(title: &str) -> CreateReminderInput {
    CreateReminderInput {
        title: title.to_string(),
        list_name: TEST_LIST.to_string(),
        priority: 0,
        due_date: None,
        notes: None,
        is_completed: false,
        completion_date: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full create → get → update → get → delete → get cycle.
#[test]
#[ignore]
fn crud_lifecycle() {
    let mut g = TestGuard::setup();

    // Create
    let r = g.create(make_input("crud lifecycle task"));
    assert_eq!(r.title, "crud lifecycle task");
    assert_eq!(r.list, TEST_LIST);
    assert!(!r.external_id.is_empty());

    // Verify it exists
    let found = g
        .find_by_eid(&r.external_id)
        .expect("reminder should exist after create");
    assert_eq!(found.title, "crud lifecycle task");

    // Update the title
    let update = ReminderUpdate {
        eid: r.external_id.clone(),
        list_name: TEST_LIST.to_string(),
        title: Some("crud lifecycle task — updated".to_string()),
        priority: None,
        is_completed: None,
        completion_date: None,
        due_date: None,
        notes: None,
    };
    g.cli
        .update_reminder(&update)
        .expect("update_reminder failed");

    let updated = g
        .find_by_eid(&r.external_id)
        .expect("reminder should exist after update");
    assert_eq!(updated.title, "crud lifecycle task — updated");

    // Delete it (remove from tracked so Drop doesn't double-delete)
    g.tracked_eids.retain(|e| e != &r.external_id);
    g.cli
        .delete_reminder(&r.external_id, TEST_LIST)
        .expect("delete_reminder failed");

    // Verify it's gone
    assert!(
        g.find_by_eid(&r.external_id).is_none(),
        "reminder should be absent after delete"
    );
}

/// Due date survives a create → read round-trip.
#[test]
#[ignore]
fn due_date_roundtrip() {
    let mut g = TestGuard::setup();

    let input = CreateReminderInput {
        due_date: Some("2026-06-15".to_string()),
        ..make_input("due date roundtrip")
    };
    let r = g.create(input);

    let found = g
        .find_by_eid(&r.external_id)
        .expect("reminder should exist");
    assert_eq!(found.due_date.as_deref(), Some("2026-06-15"));
}

/// Priority values 0, 1, 5, 9 all survive a create → read round-trip.
#[test]
#[ignore]
fn priority_roundtrip() {
    let mut g = TestGuard::setup();

    for priority in [0i32, 1, 5, 9] {
        let input = CreateReminderInput {
            priority,
            ..make_input(&format!("priority {priority} roundtrip"))
        };
        let r = g.create(input);
        let found = g
            .find_by_eid(&r.external_id)
            .expect("reminder should exist");
        assert_eq!(
            found.priority, priority,
            "priority {priority} did not round-trip correctly"
        );
    }
}

/// Multi-line notes survive a create → read round-trip.
#[test]
#[ignore]
fn notes_roundtrip() {
    let mut g = TestGuard::setup();

    let notes = "line one\nline two\nline three";
    let input = CreateReminderInput {
        notes: Some(notes.to_string()),
        ..make_input("notes roundtrip")
    };
    let r = g.create(input);

    let found = g
        .find_by_eid(&r.external_id)
        .expect("reminder should exist");
    assert_eq!(found.notes.as_deref(), Some(notes));
}

/// A due date can be cleared by updating it to `Some(None)`.
#[test]
#[ignore]
fn clear_due_date() {
    let mut g = TestGuard::setup();

    // Create with a due date
    let input = CreateReminderInput {
        due_date: Some("2026-06-15".to_string()),
        ..make_input("clear due date")
    };
    let r = g.create(input);

    // Verify due date is set
    let found = g.find_by_eid(&r.external_id).unwrap();
    assert!(found.due_date.is_some(), "due date should be set initially");

    // Clear the due date
    let update = ReminderUpdate {
        eid: r.external_id.clone(),
        list_name: TEST_LIST.to_string(),
        due_date: Some(None),
        title: None,
        priority: None,
        is_completed: None,
        completion_date: None,
        notes: None,
    };
    g.cli
        .update_reminder(&update)
        .expect("update_reminder failed");

    let cleared = g.find_by_eid(&r.external_id).unwrap();
    assert!(cleared.due_date.is_none(), "due date should be cleared");
}

/// Notes can be cleared by updating them to `Some(None)`.
#[test]
#[ignore]
fn clear_notes() {
    let mut g = TestGuard::setup();

    let input = CreateReminderInput {
        notes: Some("initial notes".to_string()),
        ..make_input("clear notes")
    };
    let r = g.create(input);

    let found = g.find_by_eid(&r.external_id).unwrap();
    assert!(found.notes.is_some(), "notes should be set initially");

    let update = ReminderUpdate {
        eid: r.external_id.clone(),
        list_name: TEST_LIST.to_string(),
        notes: Some(None),
        title: None,
        priority: None,
        is_completed: None,
        completion_date: None,
        due_date: None,
    };
    g.cli
        .update_reminder(&update)
        .expect("update_reminder failed");

    let cleared = g.find_by_eid(&r.external_id).unwrap();
    assert!(cleared.notes.is_none(), "notes should be cleared");
}

/// A reminder can be marked complete (with a date) and then uncompleted.
#[test]
#[ignore]
fn mark_complete_and_uncomplete() {
    let mut g = TestGuard::setup();

    let r = g.create(make_input("complete and uncomplete"));

    // Mark complete
    let update = ReminderUpdate {
        eid: r.external_id.clone(),
        list_name: TEST_LIST.to_string(),
        is_completed: Some(true),
        completion_date: Some(Some("2026-03-01".to_string())),
        title: None,
        priority: None,
        due_date: None,
        notes: None,
    };
    g.cli
        .update_reminder(&update)
        .expect("mark complete failed");

    let completed = g
        .find_by_eid(&r.external_id)
        .expect("should exist after completing");
    assert!(completed.is_completed, "should be completed");
    assert_eq!(completed.completion_date.as_deref(), Some("2026-03-01"));

    // Uncomplete
    let unupdate = ReminderUpdate {
        eid: r.external_id.clone(),
        list_name: TEST_LIST.to_string(),
        is_completed: Some(false),
        completion_date: Some(None),
        title: None,
        priority: None,
        due_date: None,
        notes: None,
    };
    g.cli.update_reminder(&unupdate).expect("uncomplete failed");

    let uncompleted = g
        .cli
        .get_reminders(TEST_LIST, true)
        .unwrap()
        .into_iter()
        .find(|r2| r2.external_id == r.external_id)
        .expect("should still exist after uncompleting");
    assert!(!uncompleted.is_completed, "should not be completed");
    assert!(
        uncompleted.completion_date.is_none(),
        "completion date should be cleared"
    );
}

/// Updating only the title leaves due date, notes, and priority intact.
#[test]
#[ignore]
fn update_preserves_unchanged_fields() {
    let mut g = TestGuard::setup();

    let input = CreateReminderInput {
        due_date: Some("2026-07-04".to_string()),
        notes: Some("do not erase me".to_string()),
        priority: 5,
        ..make_input("preserve fields test")
    };
    let r = g.create(input);

    // Update only the title
    let update = ReminderUpdate {
        eid: r.external_id.clone(),
        list_name: TEST_LIST.to_string(),
        title: Some("preserve fields test — renamed".to_string()),
        priority: None,
        is_completed: None,
        completion_date: None,
        due_date: None,
        notes: None,
    };
    g.cli
        .update_reminder(&update)
        .expect("update_reminder failed");

    let updated = g.find_by_eid(&r.external_id).unwrap();
    assert_eq!(updated.title, "preserve fields test — renamed");
    assert_eq!(
        updated.due_date.as_deref(),
        Some("2026-07-04"),
        "due date should be preserved"
    );
    assert_eq!(
        updated.notes.as_deref(),
        Some("do not erase me"),
        "notes should be preserved"
    );
    assert_eq!(updated.priority, 5, "priority should be preserved");
}

/// A reminder can be created in an already-completed state.
#[test]
#[ignore]
fn create_completed_reminder() {
    let mut g = TestGuard::setup();

    let input = CreateReminderInput {
        is_completed: true,
        completion_date: Some("2026-02-20".to_string()),
        ..make_input("already done")
    };
    let r = g.create(input);

    let found = g
        .cli
        .get_reminders(TEST_LIST, true)
        .unwrap()
        .into_iter()
        .find(|r2| r2.external_id == r.external_id)
        .expect("completed reminder should be found when include_completed=true");
    assert!(found.is_completed, "should be completed");
    assert_eq!(found.completion_date.as_deref(), Some("2026-02-20"));
}

/// Deleting a reminder with a bogus eid returns an error.
#[test]
#[ignore]
fn delete_nonexistent_is_error() {
    let _g = TestGuard::setup();
    let cli = SwiftCli::new().unwrap();

    let result = cli.delete_reminder("nonexistent-eid-that-does-not-exist", TEST_LIST);
    assert!(result.is_err(), "deleting a non-existent eid should fail");
}

/// The test list is created and deleted correctly around the test suite.
#[test]
#[ignore]
fn list_create_delete_idempotent() {
    let cli = SwiftCli::new().expect("SwiftCli::new() failed");

    // Create a uniquely-named temp list
    let tmp = "remtodo-tmp-list-test";
    cli.create_list(tmp)
        .expect("first create_list should succeed");

    // Creating again should be idempotent (no error)
    cli.create_list(tmp)
        .expect("second create_list should succeed (idempotent)");

    // Delete it
    cli.delete_list(tmp).expect("delete_list should succeed");

    // Deleting again should be idempotent
    cli.delete_list(tmp)
        .expect("second delete_list should succeed (idempotent)");

    // Verify it's gone
    let lists = cli.list_lists().expect("list_lists failed");
    assert!(
        !lists.iter().any(|l| l.title == tmp),
        "list should not exist after delete"
    );
}
