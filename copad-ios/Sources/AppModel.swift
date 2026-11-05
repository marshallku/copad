// The web-bridge shell is a Debug-only mode (CopadApp): store builds do not carry it.
#if DEBUG
import Foundation
import UserNotifications

/// App state: the validated server URL (UserDefaults), the bearer token (Keychain, a
/// high-authority secret) and local-notification status.
///
/// A set token is seeded into the PWA's `sessionStorage["copad.token"]` at document-start
/// (origin-scoped), so it is entered once and survives launches; when empty the PWA shows its own
/// token page.
@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var serverURL: URL?
    @Published private(set) var token: String = ""
    @Published private(set) var notifStatus: UNAuthorizationStatus = .notDetermined

    private let defaultsKey = "serverURLString"

    init() {
        // `-ServerURL` / `-Token` launch arguments override persisted state on every launch, so
        // headless verification runs are deterministic.
        let args = ProcessInfo.processInfo.arguments
        if let i = args.firstIndex(of: "-ServerURL"), i + 1 < args.count {
            UserDefaults.standard.set(args[i + 1], forKey: defaultsKey)
        }
        if let i = args.firstIndex(of: "-Token"), i + 1 < args.count {
            // Adopt the arg only on a successful write; on failure clear the slot so a stale
            // token cannot be loaded.
            if !TokenStore.save(args[i + 1]) { TokenStore.clear() }
        }
        let stored = UserDefaults.standard.string(forKey: defaultsKey) ?? ""
        serverURL = Self.validate(stored)
        token = TokenStore.load() ?? ""
    }

    /// Persist the bearer token to the Keychain; empty clears it. Returns whether the write
    /// succeeded: the caller must NOT switch origins on failure, or a stale token could be seeded
    /// into the new one. `token` is published only on success.
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

    /// Persist and apply a new server URL; false (nothing persisted) if validation fails.
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

    /// HTTPS is required except for explicit loopback dev hosts. Plain `http` to another host
    /// would send the bearer token (which can drive the workstation terminal) in cleartext, and
    /// ATS's `NSAllowsLocalNetworking` does not stop that (it permits IP-literal loads).
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
    // Only requests LOCAL-notification permission. registerForRemoteNotifications is not called:
    // real APNs needs an `aps-environment` entitlement, server support and a real device, and
    // would only hit didFailToRegister.
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
#endif
