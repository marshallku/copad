import Foundation

/// Swift port of `copad-core/src/agent_cockpit.rs`; keep the transition rules in sync with it.
/// See `docs/agent-cockpit.md`. `session` is display-only and never gates transitions.

public enum AgentState: String {
    case idle
    case working
    case awaiting
    case done


    public var needsAttention: Bool { self == .awaiting || self == .done }

    public var rank: Int {
        switch self {
        case .awaiting: 0
        case .done: 1
        case .working: 2
        case .idle: 3
        }
    }

    public var label: String {
        switch self {
        case .idle: "idle"
        case .working: "working"
        case .awaiting: "needs input"
        case .done: "done"
        }
    }
}

@MainActor
public final class AgentCockpitModel {
    private struct PaneAgent {
        var session: String?
        var state: AgentState
    }

    private var panes: [String: PaneAgent] = [:]
    private var observers: [Int: () -> Void] = [:]
    private var nextToken = 0

    public init() {}

    @discardableResult
    public func observe(kind: String, payload: [String: Any]) -> Bool {
        let newState: AgentState
        switch kind {
        case "claude.working": newState = .working
        case "claude.awaiting_input": newState = .awaiting
        case "claude.session_stopped": newState = .done
        default: return false
        }
        guard let panelID = payload["panel_id"] as? String, !panelID.isEmpty else {
            return false
        }
        var session = payload["session"] as? String
        if session?.isEmpty == true { session = nil }

        var entry = panes[panelID] ?? PaneAgent(session: nil, state: .idle)
        let changed = entry.state != newState || entry.session != session
        entry.state = newState
        entry.session = session
        panes[panelID] = entry
        return changed
    }

    @discardableResult
    public func acknowledge(_ panelID: String) -> Bool {
        guard var entry = panes[panelID], entry.state.needsAttention else { return false }
        entry.state = .idle
        panes[panelID] = entry
        return true
    }

    @discardableResult
    public func forget(_ panelID: String) -> Bool {
        panes.removeValue(forKey: panelID) != nil
    }

    public func reset() {
        for key in panes.keys {
            panes[key]?.state = .idle
        }
    }

    public func state(_ panelID: String) -> AgentState { panes[panelID]?.state ?? .idle }
    public func session(_ panelID: String) -> String? { panes[panelID]?.session }
    public var attentionCount: Int { panes.values.filter { $0.state.needsAttention }.count }

    // MARK: - Observers (weak-capturing closures; caller also removes on deinit)

    public func addObserver(_ observer: @escaping () -> Void) -> Int {
        let token = nextToken
        nextToken += 1
        observers[token] = observer
        return token
    }

    public func removeObserver(_ token: Int) { observers.removeValue(forKey: token) }

    public func notifyObservers() { for observer in observers.values { observer() } }

    public var observerCount: Int { observers.count }
}
