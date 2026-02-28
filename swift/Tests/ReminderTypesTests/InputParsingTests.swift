import XCTest

@testable import ReminderTypes

final class InputParsingTests: XCTestCase {
    // MARK: - normalizeNotes

    func testNormalizeNotesNil() {
        XCTAssertNil(normalizeNotes(nil))
    }

    func testNormalizeNotesEmpty() {
        // EventKit returns "" when notes are cleared; we normalise to nil.
        XCTAssertNil(normalizeNotes(""))
    }

    func testNormalizeNotesNonEmpty() {
        XCTAssertEqual(normalizeNotes("Buy 2% milk"), "Buy 2% milk")
    }

    func testNormalizeNotesWhitespaceOnly() {
        // Whitespace-only notes are retained — EventKit distinguishes them from "".
        XCTAssertEqual(normalizeNotes("   "), "   ")
    }

    // MARK: - clearableString

    func testClearableStringAbsent() {
        let result = clearableString(from: [:], key: "notes")
        XCTAssertFalse(result.isPresent)
        XCTAssertNil(result.value)
    }

    func testClearableStringNull() {
        // Key present with NSNull → field cleared.
        let dict: [String: Any] = ["notes": NSNull()]
        let result = clearableString(from: dict, key: "notes")
        XCTAssertTrue(result.isPresent)
        XCTAssertNil(result.value)
    }

    func testClearableStringValue() {
        let dict: [String: Any] = ["notes": "Buy oat milk"]
        let result = clearableString(from: dict, key: "notes")
        XCTAssertTrue(result.isPresent)
        XCTAssertEqual(result.value, "Buy oat milk")
    }

    func testClearableStringOtherKeyAbsent() {
        let dict: [String: Any] = ["title": "Task A"]
        let result = clearableString(from: dict, key: "notes")
        XCTAssertFalse(result.isPresent)
    }

    // MARK: - clearableDate

    func testClearableDateAbsent() {
        let result = clearableDate(from: [:], key: "dueDate")
        XCTAssertFalse(result.isPresent)
        XCTAssertNil(result.date)
    }

    func testClearableDateNull() {
        // Key present with NSNull → field cleared.
        let dict: [String: Any] = ["dueDate": NSNull()]
        let result = clearableDate(from: dict, key: "dueDate")
        XCTAssertTrue(result.isPresent)
        XCTAssertNil(result.date)
    }

    func testClearableDateValidString() {
        let dict: [String: Any] = ["dueDate": "2026-03-15"]
        let result = clearableDate(from: dict, key: "dueDate")
        XCTAssertTrue(result.isPresent)
        XCTAssertNotNil(result.date)
        XCTAssertEqual(dayFormatter.string(from: result.date!), "2026-03-15")
    }

    func testClearableDateInvalidString() {
        // Unparseable string → isPresent=true, date=nil (clears the field).
        let dict: [String: Any] = ["dueDate": "not-a-date"]
        let result = clearableDate(from: dict, key: "dueDate")
        XCTAssertTrue(result.isPresent)
        XCTAssertNil(result.date)
    }

    func testClearableDateWrongType() {
        // Non-string value → isPresent=true, date=nil (clears the field).
        let dict: [String: Any] = ["dueDate": 12345]
        let result = clearableDate(from: dict, key: "dueDate")
        XCTAssertTrue(result.isPresent)
        XCTAssertNil(result.date)
    }

    func testClearableDateEdgeDates() {
        let dates = ["2026-01-01", "2026-12-31", "2024-02-29"]
        for dateStr in dates {
            let dict: [String: Any] = ["dueDate": dateStr]
            let result = clearableDate(from: dict, key: "dueDate")
            XCTAssertTrue(result.isPresent, "Expected isPresent for \(dateStr)")
            XCTAssertNotNil(result.date, "Expected parsed date for \(dateStr)")
            XCTAssertEqual(
                dayFormatter.string(from: result.date!), dateStr,
                "Round-trip failed for \(dateStr)"
            )
        }
    }

    // MARK: - semantic equivalence

    func testClearableAbsentKeysBehaveSameForBothTypes() {
        let strResult = clearableString(from: [:], key: "k")
        let dateResult = clearableDate(from: [:], key: "k")
        XCTAssertEqual(strResult.isPresent, dateResult.isPresent)
        XCTAssertNil(strResult.value)
        XCTAssertNil(dateResult.date)
    }
}
