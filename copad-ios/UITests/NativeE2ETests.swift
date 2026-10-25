import XCTest

/// End-to-end on the simulator against a REAL comux over a REAL SSH connection: a throwaway
/// user-level sshd on the Mac and an isolated comux server, both set up by
/// `scripts/e2e-native.sh`, which passes their coordinates in `TEST_RUNNER_E2E_*`.
final class NativeE2ETests: XCTestCase {
    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        let env = ProcessInfo.processInfo.environment
        guard let host = env["E2E_HOST"] else {
            throw XCTSkip("run through scripts/e2e-native.sh")
        }
        app = XCUIApplication()
        app.launchArguments = [
            "-E2EHost", host,
            "-E2EPort", env["E2E_PORT"] ?? "22",
            "-E2EUser", env["E2E_USER"] ?? "",
            "-E2EKeySeed", env["E2E_SEED"] ?? "",
            "-E2EHostKey", env["E2E_HOSTKEY"] ?? "",
            "-E2ECommand", env["E2E_COMMAND"] ?? "",
            "-E2EAutoConnect", "YES",
            "-copad.webMode", "NO",
        ]
        app.launch()
    }

    private var terminal: XCUIElement { app.otherElements["terminal"] }

    private func screen() -> String { (terminal.value as? String) ?? "" }

    private func waitScreen(_ needle: String, timeout: TimeInterval = 15,
                            file: StaticString = #filePath, line: UInt = #line) {
        let end = Date().addingTimeInterval(timeout)
        while Date() < end {
            if screen().contains(needle) { return }
            Thread.sleep(forTimeInterval: 0.3)
        }
        XCTFail("screen never showed \(needle):\n\(screen())", file: file, line: line)
    }

    func testNativeSessionEndToEnd() throws {
        // 1. Live: the tab strip comes from L0 and the screen from the render channel.
        XCTAssertTrue(app.buttons["tab-1"].waitForExistence(timeout: 30), "never went live")
        XCTAssertTrue(terminal.waitForExistence(timeout: 5))

        // 2. The phone is the only client and claimed the chrome: comux's own sidebar is gone.
        let end = Date().addingTimeInterval(10)
        while Date() < end, screen().contains("spaces") { Thread.sleep(forTimeInterval: 0.3) }
        XCTAssertFalse(screen().contains("spaces"), "TUI sidebar still drawn:\n\(screen())")

        // 3. Typing reaches the shell (committed text → keys, Return → Enter).
        terminal.tap()
        app.typeText("echo NATIVE-$((6*7))\n")
        waitScreen("NATIVE-42")

        // 4. Hangul through the text system, committed as text.
        app.typeText("echo 한글-확인\n")
        waitScreen("한글-확인")
        snapshot("typed")

        // 5. The key bar: ^b c opens a tab, which the native tab strip shows.
        app.buttons["key-newtab"].tap()
        XCTAssertTrue(app.buttons["tab-2"].waitForExistence(timeout: 10), "^b c did not add a tab")

        // 5b. Overlays are native (decisions #170): ^b $ opens comux's rename prompt as a
        //     SHEET (the TUI never draws it), and the answer renames the session.
        app.buttons["key-prefix"].tap()
        app.typeText("$")
        let field = app.textFields["overlayText"]
        XCTAssertTrue(field.waitForExistence(timeout: 10), "rename prompt was not delegated")
        XCTAssertFalse(screen().contains("rename"), "the TUI drew the prompt too")
        field.tap()
        field.press(forDuration: 1.0)
        if app.menuItems["Select All"].waitForExistence(timeout: 2) { app.menuItems["Select All"].tap() }
        field.typeText("ios-renamed")
        app.buttons["overlayOK"].tap()
        let activeName = app.staticTexts["activeSession"]
        expectation(for: NSPredicate(format: "label CONTAINS 'ios-renamed'"), evaluatedWith: activeName)
        waitForExpectations(timeout: 10)
        snapshot("renamed")
        // ^f arrives as a native picker; filtering + Return picks the other session.
        terminal.tap()
        app.buttons["key-switcher"].tap()
        let filter = app.textFields["overlayFilter"]
        XCTAssertTrue(filter.waitForExistence(timeout: 10), "^f was not delegated")
        snapshot("switcher")
        app.buttons["overlayCancel"].tap()
        terminal.tap()

        // 5c. "New session…" asks comux to open Ctrl-b C for us (decisions #172): the directory
        //     browser is a sheet filled once comux's listing lands; descending into proj and
        //     "Create session here" lead to comux's seeded name prompt.
        app.buttons["sessionMenu"].tap()
        app.buttons["menuNewSession"].tap()
        let proj = app.buttons["overlayRow-d:2"]
        XCTAssertTrue(proj.waitForExistence(timeout: 15), "the browser sheet never listed ~/proj")
        snapshot("browser")
        proj.tap()
        let here = app.buttons["overlayRow-here"]
        XCTAssertTrue(app.staticTexts["~/proj"].waitForExistence(timeout: 15), "did not descend into proj")
        snapshot("browser-proj")
        here.tap()
        let name = app.textFields["overlayText"]
        XCTAssertTrue(name.waitForExistence(timeout: 10), "no name prompt after 'Create session here'")
        app.buttons["overlayOK"].tap()
        expectation(for: NSPredicate(format: "label == 'proj'"), evaluatedWith: app.staticTexts["activeSession"])
        waitForExpectations(timeout: 10)
        terminal.tap()

        // 6. The spaces drawer lists the other session and the fake agent; a tap switches.
        app.buttons["openSpaces"].tap()
        XCTAssertTrue(app.buttons["space-e2e-two"].waitForExistence(timeout: 10))
        XCTAssertTrue(app.descendants(matching: .any).matching(
            NSPredicate(format: "identifier BEGINSWITH 'agent-'")).firstMatch.waitForExistence(timeout: 10),
            "the fake agent is not in the drawer")
        snapshot("spaces-drawer")
        app.buttons["space-e2e-two"].tap()
        let active = app.staticTexts["activeSession"]
        let switched = NSPredicate(format: "label == 'e2e-two'")
        expectation(for: switched, evaluatedWith: active)
        waitForExpectations(timeout: 10)

        // 7. Notifications: the pre-raised one is listed; tapping it marks it read and jumps.
        app.buttons["openNotifications"].tap()
        let notif = app.descendants(matching: .any).matching(
            NSPredicate(format: "identifier BEGINSWITH 'notif-'")).firstMatch
        XCTAssertTrue(notif.waitForExistence(timeout: 10))
        snapshot("notifications")
        notif.tap()
        let back = NSPredicate(format: "label != 'e2e-two'")
        expectation(for: back, evaluatedWith: active)
        waitForExpectations(timeout: 10)

        // 8. Background and return: the session reconnects (replacing its attachment).
        XCUIDevice.shared.press(.home)
        Thread.sleep(forTimeInterval: 2)
        app.activate()
        XCTAssertTrue(app.buttons["tab-1"].waitForExistence(timeout: 30), "did not come back")
        terminal.tap()
        app.typeText("echo BACK-$((1+1))\n")
        waitScreen("BACK-2")
        snapshot("after-resume")
    }

    private func snapshot(_ name: String) {
        let a = XCTAttachment(screenshot: app.screenshot())
        a.name = name
        a.lifetime = .keepAlways
        add(a)
    }
}
