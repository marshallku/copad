import CCopadFFI
import Foundation

/// Mirrors `copad-core::plugin::PluginManifest` for macOS.
///
/// Discovery reads the same root as `copadd` (`~/Library/Application Support/copad/plugins`) so
/// the daemon and GUI agree on which plugin wins a duplicate name: sort by `(name, dir.path)`
/// and take the last, as `copad-core::plugin::discover_sorted_plugins` + `resolve_by_name` do.
enum PluginManifestStore {
    static var macOSRoot: URL {
        FileManager.default
            .homeDirectoryForCurrentUser
            .appendingPathComponent("Library")
            .appendingPathComponent("Application Support")
            .appendingPathComponent("copad")
            .appendingPathComponent("plugins")
    }

    /// Parses every `plugin.toml` under the root and dedupes by plugin name, returning winners in
    /// sorted order. Parse errors are logged to stderr and skipped.
    static func discover() -> [LoadedPluginManifest] {
        var entries: [(LoadedPluginManifest, URL)] = []
        for entry in directories(in: macOSRoot) {
            guard let loaded = parse(at: entry) else { continue }
            entries.append((loaded, entry))
        }
        entries.sort { lhs, rhs in
            let lname = lhs.0.manifest.plugin.name
            let rname = rhs.0.manifest.plugin.name
            if lname != rname { return lname < rname }
            return lhs.1.path < rhs.1.path
        }
        // Iterate the sorted array; the `byName` overwrite leaves the sorted-last winner.
        var byName: [String: LoadedPluginManifest] = [:]
        var order: [String] = []
        for (loaded, _) in entries {
            let name = loaded.manifest.plugin.name
            if byName[name] == nil { order.append(name) }
            byName[name] = loaded
        }
        return order.compactMap { byName[$0] }
    }

    private static func directories(in root: URL) -> [URL] {
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: root,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles],
        ) else { return [] }
        return entries.filter { entry in
            (try? entry.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true
        }
    }

    /// TOML parse and validation run in `copad-core::plugin` via FFI, so the manifest schema and
    /// defaults have a single source of truth in Rust; Swift only decodes the returned JSON.
    private static func parse(at dir: URL) -> LoadedPluginManifest? {
        let manifestURL = dir.appendingPathComponent("plugin.toml")
        guard FileManager.default.fileExists(atPath: manifestURL.path) else {
            return nil
        }
        guard let cstr = manifestURL.path.withCString({ copad_ffi_plugin_validate_toml($0) }) else {
            let err = copad_ffi_last_error().map { String(cString: $0) } ?? "<unknown>"
            let msg = "[copad] plugin manifest \(manifestURL.path): \(err)\n"
            FileHandle.standardError.write(Data(msg.utf8))
            return nil
        }
        defer { copad_ffi_free_string(cstr) }
        let json = String(cString: cstr)
        guard let data = json.data(using: .utf8) else { return nil }
        do {
            let manifest = try JSONDecoder().decode(PluginManifest.self, from: data)
            return LoadedPluginManifest(manifest: manifest, dir: dir)
        } catch {
            // Core just emitted this JSON, so Swift's Codable and serde's `PluginManifest`
            // have diverged.
            let msg = "[copad] plugin manifest decode (from core JSON) failed for \(manifestURL.path): \(error)\n"
            FileHandle.standardError.write(Data(msg.utf8))
            return nil
        }
    }
}

/// Discovered manifest plus its directory, which resolves relative `services.exec` paths (the
/// install layout symlinks the binary into `<dir>/<exec>`).
struct LoadedPluginManifest {
    let manifest: PluginManifest
    let dir: URL
}

// MARK: - TOML decode types

// TOMLKit's Decoder ignores `var foo: T = default`, so a missing key throws keyNotFound. The
// inits below use explicit `decodeIfPresent ?? <default>` to mirror serde's `#[serde(default)]`.

struct PluginManifest: Decodable {
    let plugin: PluginMeta
    let services: [PluginServiceDef]
    /// `[[panels]]` declarations: `name` (used by `plugin.open`), relative HTML `file`, `title`.
    let panels: [PluginPanelDef]
    /// `[[modules]]` declarations: status-bar widgets that run a shell command on a timer.
    let modules: [PluginModuleDef]

    enum CodingKeys: String, CodingKey {
        case plugin, services, panels, modules
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        plugin = try c.decode(PluginMeta.self, forKey: .plugin)
        services = try c.decodeIfPresent([PluginServiceDef].self, forKey: .services) ?? []
        panels = try c.decodeIfPresent([PluginPanelDef].self, forKey: .panels) ?? []
        modules = try c.decodeIfPresent([PluginModuleDef].self, forKey: .modules) ?? []
    }
}

struct PluginModuleDef: Decodable {
    let name: String
    let exec: String
    let interval: Int
    let position: String
    let order: Int
    let cssClass: String?

    enum CodingKeys: String, CodingKey {
        case name, exec, interval, position, order
        case cssClass = "class"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        exec = try c.decode(String.self, forKey: .exec)
        interval = try c.decodeIfPresent(Int.self, forKey: .interval) ?? 10
        position = try c.decodeIfPresent(String.self, forKey: .position) ?? "right"
        order = try c.decodeIfPresent(Int.self, forKey: .order) ?? 50
        cssClass = try c.decodeIfPresent(String.self, forKey: .cssClass)
    }
}

struct PluginPanelDef: Decodable {
    let name: String
    let title: String
    let file: String
    let icon: String?

    enum CodingKeys: String, CodingKey {
        case name, title, file, icon
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        title = try c.decode(String.self, forKey: .title)
        file = try c.decode(String.self, forKey: .file)
        icon = try c.decodeIfPresent(String.self, forKey: .icon)
    }
}

struct PluginMeta: Decodable {
    let name: String
    let title: String
    let version: String
    let description: String?
}

struct PluginServiceDef: Decodable {
    let name: String
    let exec: String
    let args: [String]
    /// Raw activation string; only `onStartup` is handled here.
    let activation: String
    let restart: String
    let provides: [String]
    let subscribes: [String]

    enum CodingKeys: String, CodingKey {
        case name, exec, args, activation, restart, provides, subscribes
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        exec = try c.decode(String.self, forKey: .exec)
        args = try c.decodeIfPresent([String].self, forKey: .args) ?? []
        activation = try c.decodeIfPresent(String.self, forKey: .activation) ?? "onStartup"
        restart = try c.decodeIfPresent(String.self, forKey: .restart) ?? "on-crash"
        provides = try c.decodeIfPresent([String].self, forKey: .provides) ?? []
        subscribes = try c.decodeIfPresent([String].self, forKey: .subscribes) ?? []
    }
}
