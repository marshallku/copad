@testable import CopadCore
import XCTest

/// Parity tests for the Swift `AgentCockpitModel`, mirroring the Rust reference
/// in `copad-core/src/agent_cockpit.rs` (search "mod tests"). Same semantics so
/// the two ports can't silently diverge: arrival-order last-write-wins, empty /
/// missing `panel_id` ignored, empty `session` → nil, acknowledge clears only
/// attention states, the change-flag covers session-only changes.
@MainActor
final class AgentCockpitModelTests: XCTestCase {
    private func ev(_ panel: String, _ session: String) -> [String: Any] {
        ["panel_id": panel, "session": session, "cwd": "/x"]
    }

    func testTransitionsMapKindsToStates() {
        let c = AgentCockpitModel()
        XCTAssertTrue(c.observe(kind: "claude.working", payload: ev("p", "s1")))
        XCTAssertEqual(c.state("p"), .working)
        XCTAssertTrue(c.observe(kind: "claude.awaiting_input", payload: ev("p", "s1")))
        XCTAssertEqual(c.state("p"), .awaiting)
        XCTAssertTrue(c.observe(kind: "claude.session_stopped", payload: ev("p", "s1")))
        XCTAssertEqual(c.state("p"), .done)
    }

    func testChangeFlagIncludesSessionOnlyChange() {
        let c = AgentCockpitModel()
        XCTAssertTrue(c.observe(kind: "claude.working", payload: ev("p", "s1")))
        XCTAssertFalse(c.observe(kind: "claude.working", payload: ev("p", "s1"))) // same
        XCTAssertTrue(c.observe(kind: "claude.working", payload: ev("p", "s2"))) // session changed
    }

    func testEmptyOrMissingPanelIDIgnored() {
        let c = AgentCockpitModel()
        XCTAssertFalse(c.observe(kind: "claude.working", payload: ev("", "s1"))) // SSH agent
        XCTAssertFalse(c.observe(kind: "claude.working", payload: ["session": "s1"]))
        XCTAssertEqual(c.state(""), .idle)
    }

    func testUnknownKindIgnored() {
        let c = AgentCockpitModel()
        XCTAssertFalse(c.observe(kind: "claude.session_started", payload: ev("p", "s1")))
        XCTAssertFalse(c.observe(kind: "notify.show", payload: ev("p", "s1")))
        XCTAssertEqual(c.state("p"), .idle)
    }

    func testAcknowledgeClearsAttentionOnly() {
        let c = AgentCockpitModel()
        c.observe(kind: "claude.awaiting_input", payload: ev("p", "s1"))
        XCTAssertTrue(c.acknowledge("p"))
        XCTAssertEqual(c.state("p"), .idle)
        XCTAssertFalse(c.acknowledge("p"))
        c.observe(kind: "claude.working", payload: ev("p", "s1"))
        XCTAssertFalse(c.acknowledge("p")) // working is not attention
        XCTAssertEqual(c.state("p"), .working)
    }

    func testForgetAndReset() {
        let c = AgentCockpitModel()
        c.observe(kind: "claude.session_stopped", payload: ev("a", "s1"))
        c.observe(kind: "claude.awaiting_input", payload: ev("b", "s2"))
        XCTAssertEqual(c.attentionCount, 2)
        XCTAssertTrue(c.forget("a"))
        XCTAssertFalse(c.forget("a"))
        XCTAssertEqual(c.state("a"), .idle)
        c.reset()
        XCTAssertEqual(c.state("b"), .idle)
        XCTAssertEqual(c.attentionCount, 0)
    }

    func testSessionRecordedForDisplayAndEmptyNormalizes() {
        let c = AgentCockpitModel()
        c.observe(kind: "claude.working", payload: ev("p", "sess-123"))
        XCTAssertEqual(c.session("p"), "sess-123")
        c.observe(kind: "claude.awaiting_input", payload: ev("p", ""))
        XCTAssertNil(c.session("p"))
    }

    func testAttentionRankOrder() {
        let states: [AgentState] = [.idle, .working, .done, .awaiting]
        XCTAssertEqual(states.sorted { $0.rank < $1.rank }, [.awaiting, .done, .working, .idle])
    }

    func testObserverRegistrationLifecycle() {
        let c = AgentCockpitModel()
        var fired = 0
        let token = c.addObserver { fired += 1 }
        XCTAssertEqual(c.observerCount, 1)
        c.notifyObservers()
        XCTAssertEqual(fired, 1)
        c.removeObserver(token)
        XCTAssertEqual(c.observerCount, 0)
        c.notifyObservers()
        XCTAssertEqual(fired, 1) // not called after removal
    }
}
