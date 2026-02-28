import Foundation

// MARK: - Notes Normalization

/// EventKit returns `""` instead of `nil` when notes are cleared.
/// Normalise to `nil` so the rest of the code sees a clean Optional.
public func normalizeNotes(_ notes: String?) -> String? {
    notes.flatMap { $0.isEmpty ? nil : $0 }
}

// MARK: - Clearable Field Helpers

/// Distinguishes between a key that is absent from the dict, present with a
/// null value, and present with a string value.
///
/// This three-way distinction maps to:
/// - `isPresent = false`: field unchanged (skip update)
/// - `isPresent = true, value = nil`: field cleared (set to nil)
/// - `isPresent = true, value = "..."`: field set to the string
public struct ClearableString {
    public let isPresent: Bool
    public let value: String?

    public init(isPresent: Bool, value: String?) {
        self.isPresent = isPresent
        self.value = value
    }
}

public func clearableString(from dict: [String: Any], key: String) -> ClearableString {
    guard dict.keys.contains(key) else {
        return ClearableString(isPresent: false, value: nil)
    }
    return ClearableString(isPresent: true, value: dict[key] as? String)
}

/// Like `ClearableString`, but additionally parses the string value as a date
/// using `dayFormatter`.
///
/// - `isPresent = false`: field unchanged (skip update)
/// - `isPresent = true, date = nil`: field cleared (key was null or value unparseable)
/// - `isPresent = true, date = <Date>`: field set to parsed date
public struct ClearableDate {
    public let isPresent: Bool
    public let date: Date?

    public init(isPresent: Bool, date: Date?) {
        self.isPresent = isPresent
        self.date = date
    }
}

public func clearableDate(from dict: [String: Any], key: String) -> ClearableDate {
    guard dict.keys.contains(key) else {
        return ClearableDate(isPresent: false, date: nil)
    }
    guard let str = dict[key] as? String else {
        // Key present but value is null (NSNull) or wrong type → clear the field.
        return ClearableDate(isPresent: true, date: nil)
    }
    return ClearableDate(isPresent: true, date: dayFormatter.date(from: str))
}
