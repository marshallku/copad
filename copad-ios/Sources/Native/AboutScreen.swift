import SwiftUI

/// Version, the privacy policy, support and the licences of what ships inside the app. The
/// policy has to be reachable from inside the app, not only from the store listing.
struct AboutScreen: View {
    @Environment(\.dismiss) private var dismiss

    static let privacyURL = URL(string: "https://copad.marshallku.dev/privacy.html")!
    static let supportURL = URL(string: "https://github.com/marshallku/copad/issues")!
    static let sourceURL = URL(string: "https://github.com/marshallku/copad")!

    private var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(short) (\(build))"
    }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    LabeledContent("Version", value: version)
                }
                Section {
                    Link("Privacy policy", destination: Self.privacyURL)
                        .accessibilityIdentifier("privacyPolicy")
                    Link("Report a problem", destination: Self.supportURL)
                    Link("Source code", destination: Self.sourceURL)
                }
                Section("licences") {
                    NavigationLink("Open-source licences") { LicencesScreen() }
                }
            }
            .navigationTitle("About")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
        }
    }
}

private struct LicencesScreen: View {
    private static let items: [(name: String, licence: String, url: String)] = [
        ("SwiftNIO", "Apache License 2.0", "https://github.com/apple/swift-nio"),
        ("SwiftNIO SSH", "Apache License 2.0", "https://github.com/apple/swift-nio-ssh"),
        ("Swift Crypto", "Apache License 2.0", "https://github.com/apple/swift-crypto"),
        ("Swift ASN.1", "Apache License 2.0", "https://github.com/apple/swift-asn1"),
        ("Swift Atomics", "Apache License 2.0", "https://github.com/apple/swift-atomics"),
        ("Swift Collections", "Apache License 2.0", "https://github.com/apple/swift-collections"),
        ("Swift System", "Apache License 2.0", "https://github.com/apple/swift-system"),
        ("JetBrains Mono Nerd Font", "SIL Open Font License 1.1", "https://github.com/ryanoasis/nerd-fonts"),
    ]

    var body: some View {
        List(Self.items, id: \.name) { item in
            Link(destination: URL(string: item.url)!) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(item.name)
                    Text(item.licence).font(.footnote).foregroundStyle(.secondary)
                }
            }
        }
        .navigationTitle("Licences")
    }
}
