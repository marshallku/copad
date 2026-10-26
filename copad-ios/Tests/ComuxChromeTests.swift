import XCTest
@testable import copad_ios

/// The phone chrome's layout rules, shared in spirit with the TUI's `split_sidebar` (#179).
final class ComuxChromeTests: XCTestCase {
    func testTheDrawerSplitGivesSlackToTheHalfThatNeedsIt() {
        XCTAssertEqual(comuxSplit(total: 600, spaces: 100, agents: 200, floor: 40, header: 20), 100)
        XCTAssertEqual(comuxSplit(total: 600, spaces: 120, agents: 2000, floor: 40, header: 20), 120)
        XCTAssertEqual(comuxSplit(total: 600, spaces: 2000, agents: 100, floor: 40, header: 20), 500)
        XCTAssertEqual(comuxSplit(total: 600, spaces: 2000, agents: 2000, floor: 40, header: 20), 300)
        // A short screen (landscape, keyboard up): spaces yields first, down to its header.
        XCTAssertEqual(comuxSplit(total: 60, spaces: 2000, agents: 2000, floor: 40, header: 20), 20)
    }

    func testResetTimesReadLikeTheTUI() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        XCTAssertEqual(untilLabel(1_000_000 + 59 * 60, now: now), "59m")
        XCTAssertEqual(untilLabel(1_000_000 + 133 * 60, now: now), "2h13m")
        XCTAssertEqual(untilLabel(1_000_000 + (5 * 24 + 19) * 3600, now: now), "5d19h")
        XCTAssertEqual(untilLabel(0, now: now), "0m")
    }

    func testASpaceWithoutABranchNamesItsFocusedCommand() {
        let pane = L0Pane(token: "t", index: 0, focused: true, label: "claude", kind: "agent", status: "idle")
        let tab = L0Tab(id: "1", index: 0, name: "", title: "x", active: true, bell: false, panes: [pane])
        var s = L0Session(id: "s", index: 0, name: "n", active: true, branch: "", dir: "/", tabs: [tab])
        XCTAssertEqual(s.subtitle, "claude")
        s.branch = "main"
        XCTAssertEqual(s.subtitle, "main")
    }
}
