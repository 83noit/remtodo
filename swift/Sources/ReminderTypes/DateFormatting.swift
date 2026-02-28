import Foundation

// MARK: - Date Formatters

public let dayFormatter: DateFormatter = {
    let f = DateFormatter()
    f.dateFormat = "yyyy-MM-dd"
    f.locale = Locale(identifier: "en_US_POSIX")
    return f
}()

public let isoFormatter = ISO8601DateFormatter()

// MARK: - Date Helpers

/// Convert a `Date` to `DateComponents` containing only year, month, and day.
///
/// Used when setting `dueDateComponents` on an `EKReminder` to ensure EventKit
/// treats the due date as date-only (no time), preventing the reminder from
/// appearing in red all day due to a spurious 00:00 time component.
public func dateOnlyComponents(from date: Date) -> DateComponents {
    Calendar.current.dateComponents([.year, .month, .day], from: date)
}
