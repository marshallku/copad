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
            "-E2EKeyP256", env["E2E_P256"] ?? "",
            "-E2EPassword", env["E2E_PASSWORD"] ?? "",
            // The enclave run meets the host key the way a user does: unknown, then trusted.
            "-E2EHostKey", enclave ? "" : env["E2E_HOSTKEY"] ?? "",
            "-E2ECommand", env["E2E_COMMAND"] ?? "",
            "-E2EAutoConnect", enclave ? "NO" : "YES",
            "-E2EKeyEnclave", enclave ? "YES" : "NO",
            "-copad.webMode", "NO",
        ]
        app.launch()
    }

    /// The e2e generates a REAL Secure Enclave key in the app (the Simulator has one on Apple
    /// silicon): its public line is read off the hosts screen into the throwaway sshd's
    /// authorized_keys before connecting (decisions #173).
    private var enclave: Bool { ProcessInfo.processInfo.environment["E2E_ENCLAVE"] == "1" }

    private func authorizeEnclaveKey() throws {
        let env = ProcessInfo.processInfo.environment
        let key = app.staticTexts["devicePublicKey"]
        XCTAssertTrue(key.waitForExistence(timeout: 10))
        let line = key.label
        XCTAssertTrue(line.hasPrefix("ecdsa-sha2-nistp256 "), "not an enclave key: \(line)")
        // Copying says so: the pasteboard itself gives no sign.
        let copy = app.buttons["copyPublicKey"]
        copy.tap()
        XCTAssertTrue(copy.label.contains("Copied"), copy.label)
        let back = expectation(for: NSPredicate(format: "label CONTAINS 'Copy public key'"),
                               evaluatedWith: copy)
        wait(for: [back], timeout: 4)
        let path = try XCTUnwrap(env["E2E_AUTHKEYS"])
        let h = try XCTUnwrap(FileHandle(forWritingAtPath: path))
        h.seekToEndOfFile()
        h.write(Data((line + "\n").utf8))
        try h.close()
        app.buttons["host-e2e"].tap()
        // First contact asks at once — a refused key used to leave the connection open until
        // the 20 s setup deadline.
        let trust = app.buttons["trustHostKey"].firstMatch
        XCTAssertTrue(trust.waitForExistence(timeout: 8), "the host key prompt took too long")
        trust.tap()
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
        if enclave { try authorizeEnclaveKey() }
        // 1. Live: the tab strip comes from L0 and the screen from the render channel.
        XCTAssertTrue(app.buttons["tab-1"].waitForExistence(timeout: 30), "never went live")
        // The first live session asks for notification permission (decisions #175).
        let allow = springboard.buttons["Allow"]
        if allow.waitForExistence(timeout: 5) { allow.tap() }
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
        // Nerd glyphs render from the bundled font (decisions #174): powerline + icons.
        app.typeText("printf 'NF \\ue0b0 \\uf121 \\uf113 \\ue0b2 END\\n'\n")
        waitScreen("END")
        Thread.sleep(forTimeInterval: 1)
        snapshot("nerd-glyphs")

        // 4b. The key bar folds the keyboard; tapping the terminal brings it back.
        app.buttons["key-hide"].tap()
        XCTAssertTrue(app.keyboards.element.waitForNonExistence(timeout: 5), "keyboard still up")
        terminal.tap()
        XCTAssertTrue(app.keyboards.element.waitForExistence(timeout: 5), "keyboard did not come back")

        // 5. The key bar: ^b c opens a tab, which the native tab strip shows.
        app.buttons["key-newtab"].tap()
        XCTAssertTrue(app.buttons["tab-2"].waitForExistence(timeout: 10), "^b c did not add a tab")

        // 5b. Overlays are native (decisions #170): ^b $ opens comux's rename prompt as a
        //     native overlay (the TUI never draws it), and the answer renames the session.
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
        //     browser is a native picker filled once comux's listing lands; descending into proj and
        //     "Create session here" lead to comux's seeded name prompt.
        app.buttons["sessionMenu"].tap()
        app.buttons["menuNewSession"].tap()
        let proj = app.buttons["overlayRow-d:2"]
        XCTAssertTrue(proj.waitForExistence(timeout: 15), "the browser never listed ~/proj")
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

        // 6. ^b e (decisions #181) opens the spaces drawer, as the TUI focuses its sidebar; it
        //    lists the other session and the fake agent, a long-press previews the agent's pane,
        //    and a tap switches.
        app.buttons["key-prefix"].tap()
        app.typeText("e")
        XCTAssertTrue(app.buttons["space-e2e-two"].waitForExistence(timeout: 10), "^b e did not open the drawer")
        let agentRow = app.descendants(matching: .any).matching(
            NSPredicate(format: "identifier BEGINSWITH 'agent-'")).firstMatch
        XCTAssertTrue(agentRow.waitForExistence(timeout: 10), "the fake agent is not in the drawer")
        snapshot("spaces-drawer")
        agentRow.press(forDuration: 1.2)
        Thread.sleep(forTimeInterval: 2)
        snapshot("drawer-preview")
        XCTAssertTrue(app.buttons["Jump to pane"].waitForExistence(timeout: 5), "no long-press menu")
        app.buttons["Jump to pane"].tap()
        app.buttons["openSpaces"].tap()
        XCTAssertTrue(app.buttons["space-e2e-two"].waitForExistence(timeout: 10))
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

        // 9. APNs (decisions #175, #177), when the script configured pushes: once comux has our
        //    token and key, background the app and let the script raise a notification for
        //    e2e-two's pane; Apple's sandbox delivers a sealed push, the notification service
        //    extension opens it (the banner shows the real body, not the placeholder), and
        //    tapping it jumps there.
        if let dir = ProcessInfo.processInfo.environment["E2E_PUSH_DIR"], !dir.isEmpty {
            let reg = URL(fileURLWithPath: dir).appendingPathComponent("comux-push.json")
            let end = Date().addingTimeInterval(30)
            while Date() < end, !((try? String(contentsOf: reg, encoding: .utf8)) ?? "").contains("\"token\"") {
                Thread.sleep(forTimeInterval: 0.5)
            }
            XCTAssertTrue(((try? String(contentsOf: reg, encoding: .utf8)) ?? "").contains("\"token\""),
                          "the app never registered for pushes")
            XCUIDevice.shared.press(.home)
            FileManager.default.createFile(atPath: dir + "/notify-now", contents: Data())
            let banner = springboard.descendants(matching: .any)
                .matching(NSPredicate(format: "label CONTAINS 'push e2e'")).firstMatch
            XCTAssertTrue(banner.waitForExistence(timeout: 60), "no push arrived")
            let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
            shot.name = "push-banner"
            shot.lifetime = .keepAlways
            add(shot)
            banner.tap()
            expectation(for: NSPredicate(format: "label == 'e2e-two'"), evaluatedWith: app.staticTexts["activeSession"])
            waitForExpectations(timeout: 30)
            snapshot("push-jumped")
        }
    }

    /// The REAL iOS Korean keyboard, key by key (`E2E_KOREAN=1`: the script makes a simulator
    /// whose only keyboard is Korean). It composes without marked text and remembers what it
    /// composed, so deleting past the held syllable rewrites the one before — which used to
    /// leave a ghost syllable drawn and the sent one in the pane.
    func testKoreanKeyboardTypesAndDeletes() throws {
        guard ProcessInfo.processInfo.environment["E2E_KOREAN"] == "1" else {
            throw XCTSkip("run with E2E_KOREAN=1")
        }
        if enclave { try authorizeEnclaveKey() }
        XCTAssertTrue(app.buttons["tab-1"].waitForExistence(timeout: 30), "never went live")
        let allow = springboard.buttons["Allow"]
        if allow.waitForExistence(timeout: 5) { allow.tap() }
        XCTAssertTrue(terminal.waitForExistence(timeout: 5))
        terminal.tap()
        // A new device's first keyboard shows the slide-to-type tip over the keys.
        let tip = app.buttons["Continue"]
        if tip.waitForExistence(timeout: 3) { tip.tap() }
        XCTAssertTrue(app.keys["ㅎ"].waitForExistence(timeout: 5), "no Korean keyboard")
        let del = app.keys["delete"]
        // What the pane has after the prompt (the held syllable is drawn, not in the grid).
        func line() -> String {
            // " % ": zsh marks a partial line with a "%" in column 0.
            let row = screen().split(separator: "\n").last { $0.contains(" % ") } ?? ""
            return row.components(separatedBy: "% ").last?.trimmingCharacters(in: .whitespaces) ?? "?"
        }
        func expect(_ want: String, _ what: String, file: StaticString = #filePath, line l: UInt = #line) {
            let end = Date().addingTimeInterval(3)
            while Date() < end, line() != want { Thread.sleep(forTimeInterval: 0.2) }
            if line() != want { snapshot("ko-fail") }
            XCTAssertEqual(line(), want, "\(what):\n\(screen())", file: file, line: l)
        }
        func type(_ keys: String) { for k in keys { app.keys[String(k)].tap() } }
        func delete(_ n: Int) { for _ in 0..<n { del.tap() } }

        // 1. 한 goes once the next syllable starts; 글 is held.
        type("ㅎㅏㄴㄱㅡㄹ")
        expect("한", "typing 한글")
        snapshot("ko-typed")
        // 2. 글 → 그 → ㄱ → gone: only the held syllable changes.
        delete(3)
        Thread.sleep(forTimeInterval: 0.5)
        expect("한", "deleting the held 글")
        // 3. The next delete decomposes 한 → 하: it leaves the pane.
        delete(1)
        expect("", "deleting into 한")
        snapshot("ko-decomposed")
        // 4. 하 → ㅎ → gone, then a delete is the pane's again: nothing is left behind.
        delete(2)
        type("ㄷㅏ")
        delete(2)
        Thread.sleep(forTimeInterval: 0.5)
        expect("", "nothing left behind")
        snapshot("ko-cleared")
        // 5. Past a space and back.
        type("ㅎㅏㄴ")
        app.keys["space"].tap()
        type("ㄱㅡㄹ")
        expect("한", "typing 한 글")
        delete(4)
        expect("한", "back to the space")
        delete(3)
        expect("", "and through 한")
        // 6. After Enter the line starts fresh: the keyboard does not resurrect the last syllable.
        type("ㅎㅏㄴ")
        app.keyboards.buttons["Return"].tap()
        delete(1)
        type("ㄱㅏ")
        app.keys["space"].tap()
        expect("가", "after Enter")
        snapshot("ko-after-enter")
    }

    /// The App Store review demo host (deploy/review-demo, `E2E_DEMO=1`): a password login, the
    /// seeded layout, and typing reaching its shell.
    func testReviewDemoPasswordLogin() throws {
        guard ProcessInfo.processInfo.environment["E2E_DEMO"] == "1" else {
            throw XCTSkip("run with E2E_DEMO=1")
        }
        XCTAssertTrue(app.buttons["tab-1"].waitForExistence(timeout: 30), "never went live")
        let allow = springboard.buttons["Allow"]
        if allow.waitForExistence(timeout: 5) { allow.tap() }
        let active = app.staticTexts["activeSession"]
        expectation(for: NSPredicate(format: "label == 'hello'"), evaluatedWith: active)
        waitForExpectations(timeout: 10)
        terminal.tap()
        app.typeText("./hello.sh\n")
        // The pane is narrow: the line wraps.
        waitScreen("Hello from")
        snapshot("demo")
    }

    private var springboard: XCUIApplication { XCUIApplication(bundleIdentifier: "com.apple.springboard") }

    private func snapshot(_ name: String) {
        let a = XCTAttachment(screenshot: app.screenshot())
        a.name = name
        a.lifetime = .keepAlways
        add(a)
    }
}
