import SwiftUI

/// Saved comux hosts, this device's public key, and the way into a session.
struct HostsScreen: View {
    @State private var hosts = HostStore.load()
    @State private var editing: MuxHost?
    @State private var session: MuxSession?
    @State private var publicKey = ""
    @State private var keyError = ""
    @State private var confirmNewKey = false
    @State private var copied = false
    @ObservedObject private var push = PushRegistry.shared
    var openWeb: () -> Void

    var body: some View {
        NavigationStack {
            List {
                Section("hosts") {
                    if hosts.isEmpty {
                        Text("Add a machine that runs comux.").foregroundStyle(.secondary)
                    }
                    ForEach(hosts) { h in
                        Button { open(h) } label: {
                            VStack(alignment: .leading) {
                                Text(h.name).font(.headline)
                                Text("\(h.user)@\(h.host):\(h.port)").font(.caption.monospaced())
                                    .foregroundStyle(.secondary)
                            }
                        }
                        .accessibilityIdentifier("host-\(h.name)")
                        .swipeActions {
                            Button("Edit") { editing = h }.tint(.blue)
                            Button("Delete", role: .destructive) {
                                PushKeys.remove(host: h.id.uuidString)
                                save(hosts.filter { $0.id != h.id })
                            }
                        }
                    }
                }
                Section {
                    Text(publicKey.isEmpty ? keyError : publicKey)
                        .font(.caption2.monospaced()).textSelection(.enabled)
                        .accessibilityIdentifier("devicePublicKey")
                    Button(action: copyKey) {
                        Label(copied ? "Copied" : "Copy public key",
                              systemImage: copied ? "checkmark" : "doc.on.doc")
                    }
                    .disabled(publicKey.isEmpty)
                    .accessibilityIdentifier("copyPublicKey")
                    ForEach(push.status.sorted(by: { $0.key < $1.key }), id: \.key) { key, text in
                        let name = hosts.first { $0.id.uuidString == key }?.name
                        Label(name.map { "\($0): \(text)" } ?? text, systemImage: "bell.slash")
                            .font(.caption).foregroundStyle(.orange)
                            .accessibilityIdentifier("pushStatus")
                    }
                    if DeviceKey.secureEnclaveAvailable && DeviceKey.kind != .secureEnclave {
                        Button("New Secure Enclave key") { confirmNewKey = true }
                            .accessibilityIdentifier("newEnclaveKey")
                    }
                } header: {
                    Text("this device's key")
                } footer: {
                    Text(DeviceKey.kind == .secureEnclave
                         ? "Add it to ~/.ssh/authorized_keys on each host. The private key lives in this device's Secure Enclave and can never be read out."
                         : "Add it to ~/.ssh/authorized_keys on each host. The private key never leaves this device.")
                }
            }
            .navigationTitle("comux")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button { editing = MuxHost(name: "", host: "", user: "") } label: { Image(systemName: "plus") }
                        .accessibilityIdentifier("addHost")
                }
                ToolbarItem(placement: .topBarLeading) {
                    Button("Web") { openWeb() }
                }
            }
            .sheet(item: $editing) { h in
                HostEditor(host: h) { saved in
                    var list = hosts.filter { $0.id != saved.id }
                    list.append(saved)
                    save(list)
                    editing = nil
                }
            }
            .fullScreenCover(item: Binding(get: { session.map(SessionBox.init) }, set: { session = $0?.s })) { box in
                SessionScreen(session: box.s, onClose: { session = nil }, onTrustKey: { h in
                    save(hosts.map { $0.id == h.id ? h : $0 })
                })
            }
            .onAppear(perform: setUp)
            // A tapped push (decisions #175): open its host, then jump to its pane.
            .onReceive(push.$tapped) { route in
                guard let route, let h = hosts.first(where: { $0.id.uuidString == route.host }) else { return }
                push.tapped = nil
                if let s = session, s.host.id == h.id {
                    if s.inputEnabled { s.jump(route.pane) } else { s.pendingJump = route.pane }
                } else {
                    let s = MuxSession(host: h)
                    s.pendingJump = route.pane
                    session = s
                }
            }
            .confirmationDialog("Replace this device's key?", isPresented: $confirmNewKey, titleVisibility: .visible) {
                Button("New Secure Enclave key", role: .destructive) {
                    do {
                        try DeviceKey.useNewSecureEnclaveKey()
                        publicKey = try DeviceKey.publicLine()
                        keyError = ""
                    } catch { keyError = error.localizedDescription }
                }
            } message: {
                Text("Hosts that trust the current key will need the new public key in authorized_keys.")
            }
        }
    }

    private func open(_ h: MuxHost) { session = MuxSession(host: h) }

    /// The pasteboard gives no sign of its own, so the button says it worked for a moment.
    private func copyKey() {
        UIPasteboard.general.string = publicKey
        UINotificationFeedbackGenerator().notificationOccurred(.success)
        withAnimation { copied = true }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            withAnimation { copied = false }
        }
    }

    private func save(_ list: [MuxHost]) {
        hosts = list
        HostStore.save(list)
    }

    private func setUp() {
        applyE2EConfig()
        do { publicKey = try DeviceKey.publicLine() } catch { keyError = error.localizedDescription }
    }

    /// Simulator e2e (scripts/e2e-ios-native.sh): launch arguments describe a throwaway sshd
    /// and the seed of a key it authorized. Ignored unless all of them are present.
    private func applyE2EConfig() {
        let d = UserDefaults.standard
        guard let host = d.string(forKey: "E2EHost"), let user = d.string(forKey: "E2EUser") else { return }
        if d.bool(forKey: "E2EKeyEnclave") {
            // Fail loudly: falling back to a key left over from another run would let the e2e
            // pass without ever using the enclave.
            do { try DeviceKey.useNewSecureEnclaveKey() } catch { fatalError("e2e enclave key: \(error)") }
        } else if let p256 = d.string(forKey: "E2EKeyP256"), !p256.isEmpty {
            try? DeviceKey.importP256(base64: p256)
        } else if let seed = d.string(forKey: "E2EKeySeed"), !seed.isEmpty {
            try? DeviceKey.importSeed(base64: seed)
        } else {
            return
        }
        var h = MuxHost(name: "e2e", host: host, port: d.integer(forKey: "E2EPort").nonZero ?? 22, user: user)
        if let cmd = d.string(forKey: "E2ECommand") { h.command = cmd }
        if let hk = d.string(forKey: "E2EHostKey"), !hk.isEmpty { h.hostKey = hk }
        if let old = hosts.first(where: { $0.name == "e2e" }) { h.id = old.id }
        save(hosts.filter { $0.name != "e2e" } + [h])
        if d.bool(forKey: "E2EAutoConnect") { open(h) }
    }
}

private struct SessionBox: Identifiable {
    let s: MuxSession
    var id: ObjectIdentifier { ObjectIdentifier(s) }
}

private extension Int {
    var nonZero: Int? { self == 0 ? nil : self }
}

struct HostEditor: View {
    @State var host: MuxHost
    var onSave: (MuxHost) -> Void
    @Environment(\.dismiss) private var dismiss
    /// Typed as text: a number-bound field keeps the last valid value when cleared, so an empty
    /// field would save the old port.
    @State private var portText: String

    init(host: MuxHost, onSave: @escaping (MuxHost) -> Void) {
        _host = State(initialValue: host)
        _portText = State(initialValue: String(host.port))
        self.onSave = onSave
    }

    private var port: Int? { Int(portText).flatMap { (1...65535).contains($0) ? $0 : nil } }

    var body: some View {
        NavigationStack {
            Form {
                TextField("Name", text: $host.name).accessibilityIdentifier("hostName")
                TextField("Host (e.g. box.tailnet.ts.net)", text: $host.host)
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                    .accessibilityIdentifier("hostAddress")
                TextField("User", text: $host.user)
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                    .accessibilityIdentifier("hostUser")
                LabeledContent("Port") {
                    TextField("22", text: $portText)
                        .keyboardType(.numberPad).multilineTextAlignment(.trailing)
                        .accessibilityIdentifier("hostPort")
                }
                Section("advanced") {
                    TextField("Remote command", text: $host.command, axis: .vertical)
                        .font(.caption.monospaced())
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                    if let k = host.hostKey {
                        LabeledContent("Host key", value: sshFingerprint(k)).font(.caption2)
                        Button("Forget host key", role: .destructive) { host.hostKey = nil }
                    }
                }
            }
            .navigationTitle(host.name.isEmpty ? "New host" : host.name)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        guard let port else { return }
                        host.port = port
                        onSave(host)
                    }
                        .disabled(host.name.isEmpty || host.host.isEmpty || host.user.isEmpty || port == nil)
                        .accessibilityIdentifier("saveHost")
                }
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            }
        }
    }
}
