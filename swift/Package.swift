// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "reminders-helper",
    platforms: [.macOS(.v13)],
    targets: [
        .target(
            name: "ReminderTypes",
            path: "Sources/ReminderTypes"
        ),
        .executableTarget(
            name: "reminders-helper",
            dependencies: ["ReminderTypes"],
            path: "Sources/reminders-helper"
        ),
        // Note: `swift test` requires Xcode (not just Command Line Tools).
        // XCTest.framework is not included in the CLT package.
        .testTarget(
            name: "ReminderTypesTests",
            dependencies: ["ReminderTypes"],
            path: "Tests/ReminderTypesTests"
        ),
    ]
)
