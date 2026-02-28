import XCTest

@testable import ReminderTypes

final class DateFormattingTests: XCTestCase {
    // MARK: - dayFormatter

    func testDayFormatterRoundTrip() {
        let date = dayFormatter.date(from: "2026-03-15")
        XCTAssertNotNil(date)
        XCTAssertEqual(dayFormatter.string(from: date!), "2026-03-15")
    }

    func testDayFormatterRejectsInvalidDate() {
        XCTAssertNil(dayFormatter.date(from: "not-a-date"))
        XCTAssertNil(dayFormatter.date(from: ""))
    }

    func testDayFormatterEdgeDates() {
        XCTAssertNotNil(dayFormatter.date(from: "2024-02-29"), "leap day should parse")
        XCTAssertNotNil(dayFormatter.date(from: "2026-12-31"))
        XCTAssertNotNil(dayFormatter.date(from: "2026-01-01"))
    }

    func testDayFormatterUsesFixedLocale() {
        // Must produce the same string regardless of system locale.
        let date = dayFormatter.date(from: "2026-07-04")!
        XCTAssertEqual(dayFormatter.string(from: date), "2026-07-04")
    }

    // MARK: - dateOnlyComponents

    func testDateOnlyComponentsStripsHourAndMinute() {
        var comps = DateComponents()
        comps.year = 2026
        comps.month = 3
        comps.day = 15
        comps.hour = 14
        comps.minute = 30
        comps.second = 45
        let date = Calendar.current.date(from: comps)!

        let result = dateOnlyComponents(from: date)
        XCTAssertEqual(result.year, 2026)
        XCTAssertEqual(result.month, 3)
        XCTAssertEqual(result.day, 15)
        XCTAssertNil(result.hour, "hour must be stripped")
        XCTAssertNil(result.minute, "minute must be stripped")
    }

    func testDateOnlyComponentsFromDayFormatterParse() {
        let date = dayFormatter.date(from: "2026-02-28")!
        let comps = dateOnlyComponents(from: date)
        XCTAssertEqual(comps.year, 2026)
        XCTAssertEqual(comps.month, 2)
        XCTAssertEqual(comps.day, 28)
        XCTAssertNil(comps.hour)
        XCTAssertNil(comps.minute)
    }

    func testDateOnlyComponentsRoundTripsViaFormatter() {
        let original = "2026-06-15"
        let date = dayFormatter.date(from: original)!
        let comps = dateOnlyComponents(from: date)
        let reconstructed = Calendar.current.date(from: comps)!
        XCTAssertEqual(dayFormatter.string(from: reconstructed), original)
    }
}
