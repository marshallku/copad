import XCTest

/// The privacy policy is reachable inside the app (App Review Guideline 5.1.1), and so are the
/// licences of what ships in it.
final class AboutTests: XCTestCase {
    func testAboutLinksThePrivacyPolicyAndListsLicences() {
        let app = XCUIApplication()
        app.launchArguments = ["-copad.webMode", "NO"]
        app.launch()
        app.buttons["about"].tap()
        XCTAssertTrue(app.buttons["privacyPolicy"].waitForExistence(timeout: 5), "no privacy policy link")
        app.buttons["Open-source licences"].tap()
        XCTAssertTrue(app.buttons["SwiftNIO SSH, Apache License 2.0"].waitForExistence(timeout: 5)
                      || app.staticTexts["SwiftNIO SSH"].exists, "licences not listed")
        let a = XCTAttachment(screenshot: app.screenshot())
        a.lifetime = .keepAlways
        add(a)
    }
}
