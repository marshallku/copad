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
            WebView(url: url, state: webState)
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
    @State private var invalid = false

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
                    Button("Connect") {
                        if model.apply(input) { isPresented = false } else { invalid = true }
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
                    Text("The bearer token is entered in the web page itself, not here — copad's web bridge owns authentication.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            .navigationTitle("copad")
            .onAppear { input = model.serverURLString }
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
