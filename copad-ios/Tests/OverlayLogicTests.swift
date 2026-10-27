import SwiftUI
import XCTest
@testable import copad_ios

/// The native overlays' keys, filters and preview decoding (decisions #180): what the TUI's
/// pickers do, so a native picker is never the less capable one.
final class OverlayLogicTests: XCTestCase {
    private func row(_ id: String, _ label: String, detail: String? = nil, pane: String? = nil,
                     session: String? = nil, noActions: Bool? = nil) -> L0PickerRow {
        L0PickerRow(id: id, label: label, detail: detail, badge: nil, current: nil, no_actions: noActions,
                    pane: pane, session: session)
    }

    private func picker(tabs: Int = 2, actions: [L0PickerAction] = [], keys: [L0PickerKey] = [],
                        noFilter: Bool = false) -> L0Picker {
        L0Picker(title: "Switch", tabs: (0..<tabs).map { L0PickerTab(name: "t\($0)", rows: []) }, tab: 0,
                 actions: actions, selected: nil, loading: false, note: "", keys: keys, noFilter: noFilter)
    }

    func testArrowsSwitchTabsAndDeclaredKeysWin() {
        let p = picker(actions: [L0PickerAction(id: "rename", label: "Rename", key: "C-r", stays: nil)],
                       keys: [L0PickerKey(key: "C-f", choice: "", stays: nil)])
        let r = row("s:a", "a")
        XCTAssertEqual(PickerLogic.outcome("Left", picker: p, row: r), .tab(-1))
        XCTAssertEqual(PickerLogic.outcome("Right", picker: p, row: r), .tab(1))
        XCTAssertEqual(PickerLogic.outcome("Down", picker: p, row: r), .move(1))
        XCTAssertEqual(PickerLogic.outcome("C-p", picker: p, row: r), .move(-1))
        XCTAssertEqual(PickerLogic.outcome("Enter", picker: p, row: r), .pick)
        XCTAssertEqual(PickerLogic.outcome("Esc", picker: p, row: r), .cancel)
        XCTAssertEqual(PickerLogic.outcome("C-r", picker: p, row: r), .answer("rename:s:a"))
        XCTAssertEqual(PickerLogic.outcome("C-f", picker: p, row: r), .cancel, "Ctrl-f closes it again")
        XCTAssertEqual(PickerLogic.outcome("C-r", picker: p, row: row("a:x", "x", noActions: true)), .none,
                       "an agent row is not renamed")
        XCTAssertEqual(PickerLogic.outcome("x", picker: p, row: r), .none, "typing belongs to the filter")
        // One tab: ←/→ are free.
        XCTAssertEqual(PickerLogic.outcome("Left", picker: picker(tabs: 1), row: r), .none)
    }

    func testTheBrowserArrowsAndTheCenterLetters() {
        let browser = picker(tabs: 2, actions: [L0PickerAction(id: "open", label: "Open", key: "Right", stays: nil)],
                             keys: [L0PickerKey(key: "Left", choice: "up", stays: nil)])
        XCTAssertEqual(PickerLogic.outcome("Left", picker: browser, row: row("d:1", "src")), .answer("up"))
        XCTAssertEqual(PickerLogic.outcome("Right", picker: browser, row: row("d:1", "src")), .answer("open:d:1"))
        let center = picker(tabs: 1, actions: [L0PickerAction(id: "dismiss", label: "Dismiss", key: "d", stays: true)],
                            keys: [L0PickerKey(key: "D", choice: "clear-all", stays: true)], noFilter: true)
        XCTAssertEqual(PickerLogic.outcome("d", picker: center, row: row("n:3", "done")), .act("dismiss:n:3"))
        XCTAssertEqual(PickerLogic.outcome("D", picker: center, row: nil), .act("clear-all"))
        XCTAssertEqual(PickerLogic.outcome("j", picker: center, row: nil), .move(1))
        XCTAssertEqual(PickerLogic.outcome("q", picker: center, row: nil), .cancel)
    }

    func testStatusFiltersKeepRowsWhoseAgentsMatch() {
        let rows = [row("s:a", "alpha", session: "a"), row("s:b", "beta", session: "b"),
                    row("a:1", "claude", detail: "webpack", pane: "p1")]
        let statuses: (L0PickerRow) -> Set<String>? = { r in
            switch r.id {
            case "s:a": return ["working"]
            case "a:1": return ["blocked"]
            default: return []
            }
        }
        XCTAssertEqual(PickerLogic.filter(rows, "!", statuses: statuses).map(\.id), ["s:a"])
        XCTAssertEqual(PickerLogic.filter(rows, "@", statuses: statuses).map(\.id), ["a:1"])
        XCTAssertEqual(PickerLogic.filter(rows, "webp", statuses: statuses).map(\.id), ["a:1"],
                       "the detail is part of what is matched")
        XCTAssertEqual(PickerLogic.filter(rows, "ah", statuses: statuses).map(\.id), ["s:a"],
                       "scattered matches still count")
    }

    func testPreviewRunsDecodeAndMapColours() throws {
        let json = ##"{"pane":"p1","cols":20,"lines":[[{"t":"ab","fg":1},{"t":"c","fg":"#010203","bold":true}]]}"##
        let p = try JSONDecoder().decode(L0Preview.self, from: Data(json.utf8))
        XCTAssertEqual(p.lines[0][0].fg, .indexed(1))
        XCTAssertEqual(p.lines[0][1].fg, .rgb(0x010203))
        XCTAssertEqual(p.lines[0][1].bold, true)
        let look = ComuxLook.overlay
        XCTAssertEqual(look.color(.indexed(1)), look.ansi[1], "0–15 come from the theme")
        XCTAssertEqual(look.color(.indexed(16)), Color(red: 0, green: 0, blue: 0))
        XCTAssertEqual(look.color(.indexed(231)), Color(red: 1, green: 1, blue: 1))
    }
}
