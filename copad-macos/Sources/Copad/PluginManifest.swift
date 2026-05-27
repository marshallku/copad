import CCopadFFI
import Foundation

/// Mirrors `copad-core::plugin::PluginManifest` for macOS.
///
/// Discovery walks the single macOS plugin root that `copadd` also reads
/// (`dirs::config_dir()/copad/plugins` = `~/Library/Application Support/
/// copad/plugins` on macOS). Same root on both sides means daemon
/// `_module.run` and macOS-side panel/statusbar lookups agree on which
/// plugin wins.
///
/// Winner rule on duplicate plugin names: sort by `(name, dir.path)` then
/// take the sorted-last entry. Mirrors `copad-core::plugin::
/// discover_sorted_plugins` + `resolve_by_name`. With a single root the
/// tie-break is trivial; the rule still must match for futureproofing.
enum PluginManifestStore {
    /// Top-level macOS plugin directory. Created lazily by the installer.
    /// Same path as `dirs::config_dir()/copad/plugins` on macOS — keeps
    /// daemon and GUI on the same plugin universe.
    static var macOSRoot: URL {
        FileManager.default
            .homeDirectoryForCurrentUser
            .appendingPathComponent("Library")
            .appendingPathComponent("Application Support")
            .appendingPathComponent("copad")
            .appendingPathComponent("plugins")
    }

    /// Walk the plugin root, parse every `plugin.toml`, dedupe by plugin
    /// name using the sort-by-(name, dir-path) + sorted-last rule. Returns
    /// winners in their sorted order so panel/statusbar traversal stays
    /// stable across runs. Parse errors are logged to stderr and skipped.
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
        // Build winners from the sorted array so traversal order is
        // deterministic + stable. `byName` overwrite gives sorted-last.
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

    /// TOML parse + validation runs in `copad-core::plugin` via FFI;
    /// Swift only decodes the returned JSON into the local model. Keeps
    /// the manifest schema, default values, and enum-string syntax
    /// (`onAction:kb.*` / `on-crash`) as the single source of truth on
    /// the Rust side.
    private static func parse(at dir: URL) -> LoadedPluginManifest? {
        let manifestURL = dir.appendingPathComponent("plugin.toml")
        // Skip subdirectories that aren't plugins — same pre-check Linux
        // `discover_plugins` uses (`manifest_path.exists()`). Without
        // this, every non-plugin dir under the plugin root would trigger
        // a `failed to read … No such file` log line from `validate_toml`.
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
            // Should not happen — core just emitted this JSON. If it
            // does, Swift's Codable and serde's `PluginManifest`
            // diverged.
            let msg = "[copad] plugin manifest decode (from core JSON) failed for \(manifestURL.path): \(error)\n"
            FileHandle.standardError.write(Data(msg.utf8))
            return nil
        }
    }
}

/// Discovered manifest + the directory it lives in. `dir` is needed to
/// resolve relative `services.exec` paths against the plugin folder
/// (the install layout symlinks the binary into `<dir>/<exec>`).
struct LoadedPluginManifest {
    let manifest: PluginManifest
    let dir: URL
}

// MARK: - TOML decode types

// IMPORTANT: TOMLKit's Decoder (like Swift's JSONDecoder) does NOT honor
// `var foo: T = default` syntax — that's a Swift-init feature, not a
// Decodable feature. A missing key throws keyNotFound regardless of the
// default. We mirror serde's `#[serde(default)]` behavior with explicit
// `decodeIfPresent ?? <default>` in the inits below.

struct PluginManifest: Decodable {
    let plugin: PluginMeta
    let services: [PluginServiceDef]
    /// PR Tier 4.1 — `[[panels]]` declarations. Each panel maps a `name`
    /// (used by `plugin.open`) to a relative HTML `file` plus a display
    /// `title`. Empty when the plugin doesn't ship any panels (echo, git).
    let panels: [PluginPanelDef]
    /// PR Tier 4.2 — `[[modules]]` declarations. Each module is a status-bar
    /// widget that runs a shell command on a timer and renders the stdout
    /// (plain text or JSON `{text, tooltip}`). Empty when the plugin
    /// doesn't ship a status bar widget.
    let modules: [PluginModuleDef]
    // commands deferred (no current macOS user).

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
    /// Relative path under the plugin directory, e.g. `panel.html`. Resolved
    /// to an absolute file URL by `PluginPanelController` at load time.
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
    /// Raw activation string from the manifest. Parsed lazily because
    /// PR 3 only handles `onStartup` — the `onAction:<glob>` and
    /// `onEvent:<glob>` variants land in PR 5 with the trigger engine.
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
