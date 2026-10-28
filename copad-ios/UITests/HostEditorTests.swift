import XCTest

/// Adding a password host the way App Review will: the password is required, saved, and not
/// shown again (the field offers to replace it).
final class HostEditorTests: XCTestCase {
    func testAPasswordHostIsSavedWithItsPassword() {
        let app = XCUIApplication()
        app.launchArguments = ["-copad.webMode", "NO"]
        app.launch()
        let name = "pw-\(Int.random(in: 1000...9999))"
        app.buttons["addHost"].tap()
        app.textFields["hostName"].tap(); app.typeText(name)
        app.textFields["hostAddress"].tap(); app.typeText("demo.example")
        app.textFields["hostUser"].tap(); app.typeText("review")
        app.buttons["hostAuth"].tap()
        app.buttons["Password"].firstMatch.tap()
        let save = app.buttons["saveHost"]
        XCTAssertFalse(save.isEnabled, "a password host needs a password")
        app.secureTextFields["hostPassword"].tap(); app.typeText("s3cret")
        XCTAssertTrue(save.isEnabled)
        save.tap()

        let row = app.buttons["host-\(name)"]
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        XCTAssertFalse(app.alerts.firstMatch.waitForExistence(timeout: 2)
                       || app.buttons["Not Now"].exists
                       || XCUIApplication(bundleIdentifier: "com.apple.springboard").buttons["Not Now"].exists,
                       "iOS offered to save the password in Passwords")
        reveal(row, "Edit", in: app).tap()
        let field = app.secureTextFields["hostPassword"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        XCTAssertEqual(field.placeholderValue, "Saved — type to replace")
        XCTAssertTrue(app.buttons["saveHost"].isEnabled, "the saved password is enough")
        app.buttons["Cancel"].tap()
        reveal(row, "Delete", in: app).tap()
        XCTAssertTrue(row.waitForNonExistence(timeout: 5))
    }

    /// A row's swipe action, once the swipe has finished opening it.
    private func reveal(_ row: XCUIElement, _ action: String, in app: XCUIApplication) -> XCUIElement {
        let button = app.buttons[action].firstMatch
        for _ in 0..<3 where !button.isHittable {
            row.swipeLeft(velocity: .slow)
            _ = button.waitForExistence(timeout: 1)
            Thread.sleep(forTimeInterval: 0.5)
        }
        return button
    }
}
