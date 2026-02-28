// reminders-helper — Swift EventKit CLI called by the Rust remtodo binary.
//
// Subcommands (selected by argv[1]):
//   fetch-list   read all reminders from a named list → JSON array on stdout
//   create-reminder   create a new reminder from JSON on stdin → JSON on stdout
//   update-reminder   apply a partial update from JSON on stdin → JSON on stdout
//   delete-reminder   delete a reminder by eid from JSON on stdin → JSON on stdout
//   batch             execute a JSON array of mixed ops from stdin → JSON array on stdout
//
// All output is UTF-8 JSON.  Errors are written to stderr as {"error":"..."} and
// the process exits with a non-zero status.  The batch subcommand defers the
// single EKEventStore commit until all operations have been processed.
import EventKit
import Foundation
import ReminderTypes

// MARK: - EventKit Helpers

/// Request Reminders access synchronously via a `DispatchSemaphore` bridge.
/// Blocks up to 30 seconds; returns `true` if access was granted, `false` otherwise.
func requestAccess(store: EKEventStore) -> Bool {
    let semaphore = DispatchSemaphore(value: 0)
    var granted = false

    store.requestAccess(to: .reminder) { g, _ in
        granted = g
        semaphore.signal()
    }
    _ = semaphore.wait(timeout: .now() + 30.0)
    return granted
}

/// Fetch all reminders from the given calendars synchronously (30 s timeout).
/// Returns `nil` if the fetch times out or the store returns no results.
func fetchReminders(store: EKEventStore, calendars: [EKCalendar]?) -> [EKReminder]? {
    let semaphore = DispatchSemaphore(value: 0)
    var result: [EKReminder]?

    let predicate = store.predicateForReminders(in: calendars)
    store.fetchReminders(matching: predicate) { reminders in
        result = reminders
        semaphore.signal()
    }
    _ = semaphore.wait(timeout: .now() + 30.0)
    return result
}

// MARK: - Error Handling

func exitWithError(_ message: String) -> Never {
    let error = ["error": message]
    if let data = try? JSONEncoder().encode(error),
       let json = String(data: data, encoding: .utf8)
    {
        FileHandle.standardError.write(json.data(using: .utf8)!)
        FileHandle.standardError.write("\n".data(using: .utf8)!)
    }
    exit(1)
}

// MARK: - I/O Helpers

func readStdinJSON() -> [String: Any] {
    let data = FileHandle.standardInput.readDataToEndOfFile()
    guard let dict = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
        exitWithError("Invalid JSON on stdin")
    }
    return dict
}

func outputJSON<T: Encodable>(_ value: T) {
    let encoder = JSONEncoder()
    guard let data = try? encoder.encode(value),
          let json = String(data: data, encoding: .utf8)
    else {
        exitWithError("Failed to encode output")
    }
    print(json)
}

// MARK: - EventKit Lookup Helpers

func findReminderInList(_ reminders: [EKReminder], eid: String) -> EKReminder? {
    reminders.first(where: { $0.calendarItemExternalIdentifier == eid })
}

func reminderToOutput(_ r: EKReminder, listName: String) -> ReminderOutput {
    let dueDate: String? = r.dueDateComponents?.date.map { dayFormatter.string(from: $0) }
    let completionDate: String? = r.completionDate.map { dayFormatter.string(from: $0) }
    let creationDate: String? = r.creationDate.map { dayFormatter.string(from: $0) }
    let lastModified: String? = r.lastModifiedDate.map { isoFormatter.string(from: $0) }
    return ReminderOutput(
        id: r.calendarItemIdentifier,
        externalId: r.calendarItemExternalIdentifier,
        title: r.title ?? "Untitled",
        dueDate: dueDate,
        priority: r.priority,
        isCompleted: r.isCompleted,
        completionDate: completionDate,
        creationDate: creationDate,
        lastModifiedDate: lastModified,
        notes: normalizeNotes(r.notes),
        list: listName
    )
}

// MARK: - Core Operation Functions
//
// Each throws OperationError on validation/lookup failures, or re-throws
// EventKit errors. When commit=false the caller is responsible for
// calling store.commit() once all operations in the batch have been applied.

func createReminderCore(store: EKEventStore, input: [String: Any], commit: Bool) throws -> BatchItemResult {
    guard let listName = input["listName"] as? String else {
        throw OperationError("listName is required")
    }
    guard let calendar = store.calendars(for: .reminder).first(where: { $0.title == listName }) else {
        throw OperationError("List not found: \(listName)")
    }

    let reminder = EKReminder(eventStore: store)
    reminder.calendar = calendar
    reminder.title = input["title"] as? String ?? "Untitled"
    reminder.priority = input["priority"] as? Int ?? 0
    reminder.isCompleted = input["isCompleted"] as? Bool ?? false

    if let dueDateStr = input["dueDate"] as? String,
       let date = dayFormatter.date(from: dueDateStr)
    {
        reminder.dueDateComponents = dateOnlyComponents(from: date)
    }

    if let completionDateStr = input["completionDate"] as? String,
       let date = dayFormatter.date(from: completionDateStr)
    {
        reminder.completionDate = date
    }

    reminder.notes = input["notes"] as? String

    try store.save(reminder, commit: commit)
    return BatchItemResult(ok: true, reminder: reminderToOutput(reminder, listName: listName))
}

func updateReminderCore(store: EKEventStore, input: [String: Any], commit: Bool) throws -> BatchItemResult {
    guard let eid = input["eid"] as? String,
          let listName = input["listName"] as? String
    else {
        throw OperationError("eid and listName are required")
    }

    guard let calendar = store.calendars(for: .reminder).first(where: { $0.title == listName }) else {
        throw OperationError("List not found: \(listName)")
    }

    guard let reminders = fetchReminders(store: store, calendars: [calendar]) else {
        throw OperationError("Failed to fetch reminders for lookup")
    }

    guard let reminder = findReminderInList(reminders, eid: eid) else {
        throw OperationError("Reminder not found: eid=\(eid)")
    }

    if let title = input["title"] as? String {
        reminder.title = title
    }
    if let priority = input["priority"] as? Int {
        reminder.priority = priority
    }
    if let isCompleted = input["isCompleted"] as? Bool {
        reminder.isCompleted = isCompleted
    }

    let dueDateResult = clearableDate(from: input, key: "dueDate")
    if dueDateResult.isPresent {
        reminder.dueDateComponents = dueDateResult.date.map { dateOnlyComponents(from: $0) }
    }

    let notesResult = clearableString(from: input, key: "notes")
    if notesResult.isPresent {
        reminder.notes = notesResult.value
    }

    let completionResult = clearableDate(from: input, key: "completionDate")
    if completionResult.isPresent {
        reminder.completionDate = completionResult.date
    }

    // Passive cleanup: strip any time component from an existing due date.
    // todo.txt is date-only; a 00:00 time makes Reminders show the item in red
    // all day. Any update is an opportunity to silently fix legacy entries.
    if var components = reminder.dueDateComponents {
        components.hour = nil
        components.minute = nil
        reminder.dueDateComponents = components
    }

    try store.save(reminder, commit: commit)
    return BatchItemResult(ok: true, reminder: reminderToOutput(reminder, listName: listName))
}

func deleteReminderCore(store: EKEventStore, input: [String: Any], commit: Bool) throws -> BatchItemResult {
    guard let eid = input["eid"] as? String,
          let listName = input["listName"] as? String
    else {
        throw OperationError("eid and listName are required")
    }

    guard let calendar = store.calendars(for: .reminder).first(where: { $0.title == listName }) else {
        throw OperationError("List not found: \(listName)")
    }

    guard let reminders = fetchReminders(store: store, calendars: [calendar]) else {
        throw OperationError("Failed to fetch reminders for lookup")
    }

    guard let reminder = findReminderInList(reminders, eid: eid) else {
        // Already gone — return success (idempotent).
        return BatchItemResult(ok: true, deleted: true)
    }

    try store.remove(reminder, commit: commit)
    return BatchItemResult(ok: true, deleted: true)
}

// MARK: - Individual Command Handlers

func createReminder(store: EKEventStore) {
    let input = readStdinJSON()
    do {
        let result = try createReminderCore(store: store, input: input, commit: true)
        outputJSON(result.reminder!)
    } catch let e as OperationError {
        exitWithError(e.message)
    } catch {
        exitWithError("Failed to save reminder: \(error.localizedDescription)")
    }
}

func updateReminder(store: EKEventStore) {
    let input = readStdinJSON()
    do {
        let result = try updateReminderCore(store: store, input: input, commit: true)
        outputJSON(result.reminder!)
    } catch let e as OperationError {
        exitWithError(e.message)
    } catch {
        exitWithError("Failed to update reminder: \(error.localizedDescription)")
    }
}

func deleteReminder(store: EKEventStore) {
    let input = readStdinJSON()
    do {
        _ = try deleteReminderCore(store: store, input: input, commit: true)
        struct DeleteResult: Encodable { let deleted: Bool }
        outputJSON(DeleteResult(deleted: true))
    } catch let e as OperationError {
        exitWithError(e.message)
    } catch {
        exitWithError("Failed to delete reminder: \(error.localizedDescription)")
    }
}

func createList(store: EKEventStore) {
    let input = readStdinJSON()

    guard let title = input["title"] as? String else {
        exitWithError("title is required")
    }

    // Idempotent: return existing list if found.
    if let existing = store.calendars(for: .reminder).first(where: { $0.title == title }) {
        outputJSON(ReminderListOutput(id: existing.calendarIdentifier, title: existing.title))
        return
    }

    let calendar = EKCalendar(for: .reminder, eventStore: store)
    calendar.title = title

    // Use the source of the default calendar as the parent.
    if let source = store.defaultCalendarForNewReminders()?.source {
        calendar.source = source
    } else if let source = store.sources.first(where: { $0.sourceType == .calDAV || $0.sourceType == .local }) {
        calendar.source = source
    } else {
        exitWithError("No suitable calendar source found")
    }

    do {
        try store.saveCalendar(calendar, commit: true)
    } catch {
        exitWithError("Failed to create list: \(error.localizedDescription)")
    }

    outputJSON(ReminderListOutput(id: calendar.calendarIdentifier, title: calendar.title))
}

func deleteList(store: EKEventStore) {
    let input = readStdinJSON()

    guard let title = input["title"] as? String else {
        exitWithError("title is required")
    }

    // Idempotent: if list doesn't exist, succeed silently.
    guard let calendar = store.calendars(for: .reminder).first(where: { $0.title == title }) else {
        struct DeleteResult: Encodable { let deleted: Bool }
        outputJSON(DeleteResult(deleted: true))
        return
    }

    do {
        try store.removeCalendar(calendar, commit: true)
    } catch {
        exitWithError("Failed to delete list: \(error.localizedDescription)")
    }

    struct DeleteResult: Encodable { let deleted: Bool }
    outputJSON(DeleteResult(deleted: true))
}

// MARK: - Read Commands

func listLists(store: EKEventStore) {
    let calendars = store.calendars(for: .reminder)
    let output = calendars.map { ReminderListOutput(id: $0.calendarIdentifier, title: $0.title) }
    outputJSON(output)
}

func getReminders(store: EKEventStore, listName: String, includeCompleted: Bool) {
    let calendars = store.calendars(for: .reminder)
    guard let calendar = calendars.first(where: { $0.title == listName }) else {
        exitWithError("List not found: \(listName)")
    }

    guard let reminders = fetchReminders(store: store, calendars: [calendar]) else {
        exitWithError("Failed to fetch reminders")
    }

    let filtered = includeCompleted ? reminders : reminders.filter { !$0.isCompleted }
    let output = filtered.map { reminderToOutput($0, listName: calendar.title) }
    outputJSON(output)
}

// MARK: - Batch Command Handler

/// Handle the `batch` subcommand: read a JSON array of operations from stdin,
/// execute each one in order, then flush a single EKEventStore commit.
///
/// Each operation produces one `BatchItemResult` in the output array (same order).
/// An individual operation failure sets `ok = false` for that result but does not
/// prevent subsequent operations from running.  All changes accumulated before a
/// failure are still committed at the end.
func handleBatch(store: EKEventStore) {
    let data = FileHandle.standardInput.readDataToEndOfFile()
    guard let ops = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] else {
        exitWithError("Invalid JSON array on stdin for batch command")
    }

    var results: [BatchItemResult] = []
    var hasChanges = false

    for op in ops {
        guard let opName = op["op"] as? String else {
            results.append(BatchItemResult(ok: false, error: "missing op field"))
            continue
        }

        switch opName {
        case "create-reminder":
            do {
                let result = try createReminderCore(store: store, input: op, commit: false)
                results.append(result)
                hasChanges = true
            } catch let e as OperationError {
                results.append(BatchItemResult(ok: false, error: e.message))
            } catch {
                results.append(BatchItemResult(ok: false, error: error.localizedDescription))
            }

        case "update-reminder":
            do {
                let result = try updateReminderCore(store: store, input: op, commit: false)
                results.append(result)
                hasChanges = true
            } catch let e as OperationError {
                results.append(BatchItemResult(ok: false, error: e.message))
            } catch {
                results.append(BatchItemResult(ok: false, error: error.localizedDescription))
            }

        case "delete-reminder":
            do {
                let result = try deleteReminderCore(store: store, input: op, commit: false)
                results.append(result)
                // Mark hasChanges only when we actually issued a remove (not "already gone").
                // The idempotent path returns ok=true,deleted=true but commit:false was never
                // called on anything — safe to skip commit in that case.
                hasChanges = true
            } catch let e as OperationError {
                results.append(BatchItemResult(ok: false, error: e.message))
            } catch {
                results.append(BatchItemResult(ok: false, error: error.localizedDescription))
            }

        default:
            results.append(BatchItemResult(ok: false, error: "unknown op: \(opName)"))
        }
    }

    // Commit all pending EventKit changes in a single write.
    if hasChanges {
        do {
            try store.commit()
        } catch {
            exitWithError("Batch commit failed: \(error.localizedDescription)")
        }
    }

    outputJSON(results)
}

// MARK: - Main

let args = CommandLine.arguments
guard args.count >= 2 else {
    exitWithError("Usage: reminders-helper <list-lists|get-reminders|create-reminder|update-reminder|delete-reminder|create-list|delete-list|batch> [options]")
}

let store = EKEventStore()
guard requestAccess(store: store) else {
    exitWithError("Access to Reminders was denied. Grant access in System Settings > Privacy & Security > Reminders.")
}

switch args[1] {
case "list-lists":
    listLists(store: store)

case "get-reminders":
    var listName = "Tasks"
    var includeCompleted = false
    var i = 2
    while i < args.count {
        switch args[i] {
        case "--list":
            i += 1
            guard i < args.count else { exitWithError("--list requires a value") }
            listName = args[i]
        case "--include-completed":
            includeCompleted = true
        default:
            exitWithError("Unknown option: \(args[i])")
        }
        i += 1
    }
    getReminders(store: store, listName: listName, includeCompleted: includeCompleted)

case "create-reminder":
    createReminder(store: store)

case "update-reminder":
    updateReminder(store: store)

case "delete-reminder":
    deleteReminder(store: store)

case "create-list":
    createList(store: store)

case "delete-list":
    deleteList(store: store)

case "batch":
    handleBatch(store: store)

default:
    exitWithError("Unknown command: \(args[1]). Use list-lists, get-reminders, create-reminder, update-reminder, delete-reminder, create-list, delete-list, or batch")
}
