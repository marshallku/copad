import Foundation
import UserNotifications

/// App state: the validated server URL (persisted in UserDefaults) + the bearer
/// token (persisted in the **Keychain** — a high-authority secret) + local-
/// notification status.
///
/// The token is optional and additive: when set, the WebView seeds it into the
/// PWA's `sessionStorage["copad.token"]` at document-start (origin-scoped), so a
/// native-app user enters it once and the app remembers it across launches
/// (sessionStorage alone dies with the app). When empty, the PWA falls back to
/// its own token page — no duplicate, no forced double-entry.
@MainActor
final class AppModel: ObservableObject {
    /// The validated, ready-to-load server URL. `nil` → show settings.
    @Published private(set) var serverURL: URL?
    /// The bearer token to seed into the PWA. Empty → let the PWA prompt.
    @Published private(set) var token: String = ""
    @Published private(set) var notifStatus: UNAuthorizationStatus = .notDetermined

    private let defaultsKey = "serverURLString"

    init() {
        // `-ServerURL` / `-Token` launch arguments override persisted state on
        // EVERY launch, so headless verification runs are deterministic
        // regardless of what a prior install left behind.
        let args = ProcessInfo.processInfo.arguments
        if let i = args.firstIndex(of: "-ServerURL"), i + 1 < args.count {
            UserDefaults.standard.set(args[i + 1], forKey: defaultsKey)
        }
        if let i = args.firstIndex(of: "-Token"), i + 1 < args.count {
            // Deterministic override: adopt the arg only on a successful write;
            // on failure clear the slot so a stale prior token can't be loaded.
            if !TokenStore.save(args[i + 1]) { TokenStore.clear() }
        }
        let stored = UserDefaults.standard.string(forKey: defaultsKey) ?? ""
        serverURL = Self.validate(stored)
        token = TokenStore.load() ?? ""
    }

    /// Persist + apply the bearer token to the Keychain. Empty clears it.
    /// Returns whether the Keychain write succeeded — the caller must NOT switch
    /// origins on failure, or the stale token could be seeded into the new one.
    /// Only publishes the in-memory token on success, so `token` never claims a
    /// value that won't survive a relaunch.
    @discardableResult
    func applyToken(_ raw: String) -> Bool {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            guard TokenStore.clear() else { return false }
            token = ""
        } else {
            guard TokenStore.save(trimmed) else { return false }
            token = trimmed
        }
        return true
    }

    var serverURLString: String {
        UserDefaults.standard.string(forKey: defaultsKey) ?? ""
    }

    /// Persist + apply a new server URL. Returns false (and does not persist)
    /// if it fails validation.
    @discardableResult
    func apply(_ raw: String) -> Bool {
        guard let url = Self.validate(raw) else { return false }
        UserDefaults.standard.set(raw.trimmingCharacters(in: .whitespacesAndNewlines),
                                  forKey: defaultsKey)
        serverURL = url
        return true
    }

    func clear() {
        UserDefaults.standard.removeObject(forKey: defaultsKey)
        serverURL = nil
    }

    /// HTTPS is required except for explicit loopback dev hosts. A plain-`http`
    /// URL to an arbitrary host would send the PWA's bearer token — which can
    /// drive the workstation terminal — over cleartext; ATS's
    /// `NSAllowsLocalNetworking` does not stop that (it permits IP-literal
    /// loads). So the trust decision lives here, not only in ATS.
    static func validate(_ raw: String) -> URL? {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let url = URL(string: trimmed),
              let scheme = url.scheme?.lowercased(),
              let host = url.host?.lowercased()
        else { return nil }
        if scheme == "https" { return url }
        let loopback: Set<String> = ["127.0.0.1", "::1", "localhost"]
        if scheme == "http", loopback.contains(host) { return url }
        return nil
    }

    // MARK: - Local notifications (permission scaffold only)
    //
    // v1 requests LOCAL-notification permission and surfaces the status. It does
    // NOT call registerForRemoteNotifications — real APNs needs an
    // `aps-environment` entitlement + server support (web-bridge WU2b) + a real
    // device, and calling it now would only hit didFailToRegister. Remote push
    // is shown as "pending" in the UI.
    func refreshNotifStatus() async {
        let settings = await UNUserNotificationCenter.current().notificationSettings()
        notifStatus = settings.authorizationStatus
    }

    func requestNotifications() async {
        _ = try? await UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound, .badge])
        await refreshNotifStatus()
    }
}
