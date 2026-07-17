import SwiftUI
import UserNotifications

struct ContentView: View {
    @StateObject private var model = AppModel()
    @StateObject private var webState = WebViewState()
    @State private var showSettings = false

    var body: some View {
        Group {
            if let url = model.serverURL {
                webScreen(url)
            } else {
                SettingsScreen(model: model, isPresented: $showSettings)
            }
        }
        .task { await model.refreshNotifStatus() }
    }

    @ViewBuilder
    private func webScreen(_ url: URL) -> some View {
        ZStack(alignment: .top) {
            WebView(url: url, state: webState, token: model.token)
                .ignoresSafeArea(.container, edges: .bottom)
            if webState.failed {
                failureBanner(url)
            }
            HStack {
                Spacer()
                Button {
                    showSettings = true
                } label: {
                    Image(systemName: "gearshape").padding(10)
                }
                .accessibilityIdentifier("settingsButton")
            }
        }
        .sheet(isPresented: $showSettings) {
            SettingsScreen(model: model, isPresented: $showSettings)
        }
    }

    private func failureBanner(_ url: URL) -> some View {
        VStack(spacing: 8) {
            Text("Couldn't load \(url.host ?? url.absoluteString)")
                .font(.headline)
            Text(webState.failureMessage)
                .font(.caption).foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            HStack {
                Button("Retry") { webState.retry() }
                    .buttonStyle(.borderedProminent)
                Button("Settings") { showSettings = true }
            }
        }
        .padding()
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .padding()
    }
}

/// Server URL entry + a notification-permission scaffold. No token field —
/// the web-bridge PWA owns auth.
struct SettingsScreen: View {
    @ObservedObject var model: AppModel
    @Binding var isPresented: Bool
    @State private var input: String = ""
    @State private var tokenInput: String = ""
    @State private var invalid = false
    @State private var tokenError = false

    var body: some View {
        NavigationStack {
            Form {
                Section("Server") {
                    TextField("https://host.your-tailnet.ts.net", text: $input)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                        .accessibilityIdentifier("serverURLField")
                    if invalid {
                        Text("Enter an https:// URL (http:// allowed only for localhost).")
                            .font(.caption).foregroundStyle(.red)
                    }
                    SecureField("bearer token (optional)", text: $tokenInput)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("tokenField")
                    if tokenError {
                        Text("Couldn't save the token to the Keychain. Server unchanged.")
                            .font(.caption).foregroundStyle(.red)
                    }
                    Button("Connect") {
                        // Order matters for the token-origin guarantee: (1) reject
                        // a bad URL before touching anything; (2) persist the token
                        // and only proceed if that SUCCEEDED — otherwise switching
                        // origins would seed a stale token into the new one; (3)
                        // apply the URL. Both model mutations run synchronously so
                        // SwiftUI coalesces them into one reload carrying (new URL,
                        // new token).
                        guard AppModel.validate(input) != nil else { invalid = true; return }
                        guard model.applyToken(tokenInput) else { tokenError = true; return }
                        tokenError = false
                        _ = model.apply(input)
                        isPresented = false
                    }
                    .accessibilityIdentifier("connectButton")
                }
                Section("Notifications") {
                    LabeledContent("Permission", value: statusText)
                    if model.notifStatus == .notDetermined {
                        Button("Enable notifications") {
                            Task { await model.requestNotifications() }
                        }
                    }
                    Text("Remote push (agent alerts while away) is pending — it needs a device build + server APNs support.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section {
                    Text("The token is stored in the iOS Keychain and seeded into the page so you don't retype it each launch. Leave it blank to use the web page's own token prompt instead.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            .navigationTitle("copad")
            .onAppear {
                input = model.serverURLString
                tokenInput = model.token
            }
        }
    }

    private var statusText: String {
        switch model.notifStatus {
        case .authorized, .provisional, .ephemeral: return "Granted (local)"
        case .denied: return "Denied"
        default: return "Not requested"
        }
    }
}
