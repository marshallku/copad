@testable import CopadCore
import Foundation
import XCTest

/// Parity tests for the Swift `ContextService` + `PaneContext`. Mirrors
/// the Rust core tests in `copad-core/src/context.rs` (search "mod tests"
/// + "pane_context_*"). Same semantics: empty `panel_id` drops the event,
/// `panel.exited` drops both cwd and pane_context for that panel,
/// `snapshot()` derives `pane_context` from the active panel only,
/// missing fields default to empty / zero.
///
/// Why mirror rather than share: Swift can't import Rust types over FFI
/// cheaply enough for a unit test surface. The doc-comments on the
/// production side point at the Rust source so a future drift is
/// catchable in code review; these tests pin the wire-shape semantics.
final class ContextServiceTests: XCTestCase {
    // MARK: - PaneContext parsing

    func testPaneContextParsesFullDictionary() {
        let payload: [String: Any] = [
            "panel_id": "p1",
            "host": "arch",
            "cwd": "/home/x/dev",
            "git_remote": "owner/repo",
            "branch": "master",
            "tmux_session": "main",
            "pane_cmd": "zsh",
            "timestamp_ms": 1_748_419_200_000,
            "v": 1,
        ]
        let ctx = PaneContext(payload: payload)
        XCTAssertNotNil(ctx)
        XCTAssertEqual(ctx?.panelID, "p1")
        XCTAssertEqual(ctx?.host, "arch")
        XCTAssertEqual(ctx?.cwd, "/home/x/dev")
        XCTAssertEqual(ctx?.gitRemote, "owner/repo")
        XCTAssertEqual(ctx?.branch, "master")
        XCTAssertEqual(ctx?.tmuxSession, "main")
        XCTAssertEqual(ctx?.paneCmd, "zsh")
        XCTAssertEqual(ctx?.timestampMs, 1_748_419_200_000)
        XCTAssertEqual(ctx?.version, 1)
    }

    func testPaneContextDefaultsMissingFields() {
        // Rust uses #[serde(default)] per field — missing fields default
        // to empty / zero, not `nil`. Mirror that exactly.
        let payload: [String: Any] = ["panel_id": "p1"]
        let ctx = PaneContext(payload: payload)
        XCTAssertEqual(ctx?.panelID, "p1")
        XCTAssertEqual(ctx?.host, "")
        XCTAssertEqual(ctx?.cwd, "")
        XCTAssertEqual(ctx?.gitRemote, "")
        XCTAssertEqual(ctx?.branch, "")
        XCTAssertEqual(ctx?.tmuxSession, "")
        XCTAssertEqual(ctx?.paneCmd, "")
        XCTAssertEqual(ctx?.timestampMs, 0)
        XCTAssertEqual(ctx?.version, 0)
    }

    func testPaneContextForwardCompatExtraFieldsIgnored() {
        let payload: [String: Any] = [
            "panel_id": "p1",
            "cwd": "/x",
            "git_remote": "owner/repo",
            "timestamp_ms": 1000,
            "v": 1,
            "future_field": "ignored",
            "another": 42,
        ]
        let ctx = PaneContext(payload: payload)
        XCTAssertEqual(ctx?.cwd, "/x")
        XCTAssertEqual(ctx?.gitRemote, "owner/repo")
        XCTAssertEqual(ctx?.timestampMs, 1000)
        XCTAssertEqual(ctx?.version, 1)
    }

    // MARK: - ContextService.apply pane.context_changed

    func testPaneContextChangedRecordsPayloadPerPanel() {
        let svc = ContextService()
        svc.apply(eventKind: "pane.context_changed", data: samplePayload(panelID: "p1"))
        let stored = svc.paneContext(panelID: "p1")
        XCTAssertNotNil(stored)
        XCTAssertEqual(stored?.cwd, "/home/x/dev")
        // Other panels unaffected.
        XCTAssertNil(svc.paneContext(panelID: "p2"))
    }

    func testPaneContextReplacesOnSecondEvent() {
        let svc = ContextService()
        svc.apply(eventKind: "pane.context_changed", data: samplePayload(panelID: "p1"))
        var second = samplePayload(panelID: "p1")
        second["cwd"] = "/tmp"
        second["git_remote"] = ""
        svc.apply(eventKind: "pane.context_changed", data: second)
        let stored = svc.paneContext(panelID: "p1")
        XCTAssertEqual(stored?.cwd, "/tmp")
        XCTAssertEqual(stored?.gitRemote, "")
    }

    func testActivePaneContextResolvesViaActivePanel() {
        let svc = ContextService()
        svc.apply(eventKind: "pane.context_changed", data: samplePayload(panelID: "p1"))
        // No active panel yet.
        XCTAssertNil(svc.activePaneContext())
        svc.apply(eventKind: "panel.focused", data: ["panel_id": "p1"])
        let active = svc.activePaneContext()
        XCTAssertEqual(active?.panelID, "p1")
        XCTAssertEqual(active?.cwd, "/home/x/dev")
        // Snapshot mirrors the active query.
        let snap = svc.snapshot()
        let snapCtx = snap["pane_context"] as? [String: Any]
        XCTAssertNotNil(snapCtx)
        XCTAssertEqual(snapCtx?["panel_id"] as? String, "p1")
        XCTAssertEqual(snapCtx?["cwd"] as? String, "/home/x/dev")
    }

    func testPanelExitedDropsPaneContextEntry() {
        let svc = ContextService()
        svc.apply(eventKind: "pane.context_changed", data: samplePayload(panelID: "p1"))
        svc.apply(eventKind: "panel.focused", data: ["panel_id": "p1"])
        svc.apply(eventKind: "panel.exited", data: ["panel_id": "p1"])
        XCTAssertNil(svc.paneContext(panelID: "p1"))
        XCTAssertNil(svc.activePaneContext())
    }

    func testPaneContextEmptyPanelIDIsDropped() {
        let svc = ContextService()
        var payload = samplePayload(panelID: "")
        payload["cwd"] = "/somewhere"
        svc.apply(eventKind: "pane.context_changed", data: payload)
        // Nothing recorded.
        XCTAssertNil(svc.paneContext(panelID: ""))
        XCTAssertNil(svc.activePaneContext())
    }

    func testPaneContextNonDictPayloadIgnored() {
        let svc = ContextService()
        // String, array, NSNumber — none are dicts. Bus carries
        // serde_json::Value-shaped payloads (object/array/scalar/null);
        // non-object should silently no-op.
        svc.apply(eventKind: "pane.context_changed", data: "not-a-dict")
        svc.apply(eventKind: "pane.context_changed", data: [1, 2, 3])
        svc.apply(eventKind: "pane.context_changed", data: NSNumber(value: 42))
        svc.apply(eventKind: "pane.context_changed", data: nil)
        XCTAssertNil(svc.activePaneContext())
    }

    // MARK: - snapshot wire shape

    func testSnapshotOmitsPaneContextWhenNoneRecorded() {
        let svc = ContextService()
        svc.apply(eventKind: "panel.focused", data: ["panel_id": "p1"])
        let snap = svc.snapshot()
        XCTAssertEqual(snap["active_panel"] as? String, "p1")
        XCTAssertNil(snap["pane_context"])
    }

    func testSnapshotAlwaysEmitsPresence() {
        // Rust `Context` always serializes `presence` (default "active");
        // Swift snapshot mirrors that shape so consumers see identical
        // wire bytes whether or not a setter has fired.
        let svc = ContextService()
        XCTAssertEqual(svc.snapshot()["presence"] as? String, "active")
        // After flipping, the snapshot reflects the new state.
        let prev = svc.setPresence(.away)
        XCTAssertEqual(prev, .active)
        XCTAssertEqual(svc.snapshot()["presence"] as? String, "away")
        XCTAssertEqual(svc.currentPresence(), .away)
    }

    func testPresenceOrthogonalToPanelState() {
        // Mirror of copad-core test `presence_orthogonal_to_panel_state`
        // — panel.exited must not reset presence.
        let svc = ContextService()
        svc.apply(eventKind: "panel.focused", data: ["panel_id": "p1"])
        svc.setPresence(.away)
        XCTAssertEqual(svc.snapshot()["active_panel"] as? String, "p1")
        XCTAssertEqual(svc.snapshot()["presence"] as? String, "away")
        svc.apply(eventKind: "panel.exited", data: ["panel_id": "p1"])
        XCTAssertEqual(svc.currentPresence(), .away)
    }

    func testSnapshotOmitsPaneContextForBackgroundPanel() {
        let svc = ContextService()
        // p1 has a pane_context but is NOT the active panel — snapshot
        // surfaces only the active panel's derived pane_context.
        svc.apply(eventKind: "pane.context_changed", data: samplePayload(panelID: "p1"))
        svc.apply(eventKind: "panel.focused", data: ["panel_id": "p2"])
        let snap = svc.snapshot()
        XCTAssertEqual(snap["active_panel"] as? String, "p2")
        XCTAssertNil(snap["pane_context"])
    }

    // MARK: - Helpers

    private func samplePayload(panelID: String) -> [String: Any] {
        [
            "panel_id": panelID,
            "host": "arch",
            "cwd": "/home/x/dev",
            "git_remote": "owner/repo",
            "branch": "master",
            "tmux_session": "",
            "pane_cmd": "zsh",
            "timestamp_ms": 1_748_419_200_000,
            "v": 1,
        ]
    }
}
