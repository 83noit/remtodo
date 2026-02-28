import Foundation

// MARK: - Operation Error

/// A simple error type for reminder helper operations, carrying a human-readable message.
public struct OperationError: Error {
    public let message: String

    public init(_ message: String) {
        self.message = message
    }
}

// MARK: - Output Types

public struct ReminderListOutput: Codable {
    public let id: String
    public let title: String

    public init(id: String, title: String) {
        self.id = id
        self.title = title
    }
}

public struct ReminderOutput: Codable {
    public let id: String
    public let externalId: String
    public let title: String
    public let dueDate: String?
    public let priority: Int
    public let isCompleted: Bool
    public let completionDate: String?
    public let creationDate: String?
    public let lastModifiedDate: String?
    public let notes: String?
    public let list: String

    public init(
        id: String,
        externalId: String,
        title: String,
        dueDate: String?,
        priority: Int,
        isCompleted: Bool,
        completionDate: String?,
        creationDate: String?,
        lastModifiedDate: String?,
        notes: String?,
        list: String
    ) {
        self.id = id
        self.externalId = externalId
        self.title = title
        self.dueDate = dueDate
        self.priority = priority
        self.isCompleted = isCompleted
        self.completionDate = completionDate
        self.creationDate = creationDate
        self.lastModifiedDate = lastModifiedDate
        self.notes = notes
        self.list = list
    }
}

// MARK: - Batch Types

/// Per-operation result returned by the `batch` subcommand.
///
/// Fields present depend on the operation type:
/// - create-reminder / update-reminder: `ok=true, reminder=<ReminderOutput>`
/// - delete-reminder: `ok=true, deleted=true`
/// - any failure: `ok=false, error="..."`
public struct BatchItemResult: Encodable {
    public let ok: Bool
    public let reminder: ReminderOutput?
    public let deleted: Bool?
    public let error: String?

    public init(ok: Bool, reminder: ReminderOutput? = nil, deleted: Bool? = nil, error: String? = nil) {
        self.ok = ok
        self.reminder = reminder
        self.deleted = deleted
        self.error = error
    }
}
