import AppKit
import Foundation

/// Status-bar module timer that fires `_module.run` against the daemon, which owns shell spawn
/// and env injection so macOS and Linux behave the same.
///
/// `runOnce` skips a tick while the previous RPC is in flight, so overlapping calls cannot let a
/// stale response overwrite a newer label. `@unchecked Sendable`: the timer and the daemon
/// completion run off-main; label writes hop to main explicitly.
final class StatusModuleRunner: @unchecked Sendable {
    private nonisolated(unsafe) weak var label: NSTextField?
    private let pluginName: String
    private let moduleName: String
    private let interval: Int
    private let daemonClient: DaemonClient
    private let queue: DispatchQueue
    private var timer: DispatchSourceTimer?
    private nonisolated(unsafe) var stopped = false

    private let inFlightLock = NSLock()
    private var inFlight = false

    init(
        label: NSTextField,
        pluginName: String,
        moduleName: String,
        interval: Int,
        daemonClient: DaemonClient,
    ) {
        self.label = label
        self.pluginName = pluginName
        self.moduleName = moduleName
        self.interval = max(1, interval)
        self.daemonClient = daemonClient
        queue = DispatchQueue(label: "copad.statusbar.\(pluginName).\(moduleName)", qos: .utility)
    }

    func start() {
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now(), repeating: .seconds(interval))
        t.setEventHandler { [weak self] in
            self?.runOnce()
        }
        t.resume()
        timer = t
    }

    func stop() {
        stopped = true
        timer?.cancel()
        timer = nil
    }

    private func runOnce() {
        if stopped { return }
        inFlightLock.lock()
        guard !inFlight else { inFlightLock.unlock(); return }
        inFlight = true
        inFlightLock.unlock()

        let labelBox = SendableBox(label)
        let plugin = pluginName
        let module = moduleName
        daemonClient.forward(
            method: "_module.run",
            params: ["plugin": plugin, "module": module],
        ) { [weak self] result in
            defer {
                self?.inFlightLock.lock()
                self?.inFlight = false
                self?.inFlightLock.unlock()
            }
            if let err = result as? RPCError {
                FileHandle.standardError.write(Data("[copad] statusbar \(plugin).\(module) daemon-forward failed: \(err.code) — \(err.message)\n".utf8))
                return
            }
            guard let dict = result as? [String: Any],
                  let stdout = dict["stdout"] as? String
            else { return }
            let (text, tooltip) = Self.parseOutput(stdout)
            DispatchQueue.main.async {
                guard let label = labelBox.value else { return }
                label.stringValue = text
                label.toolTip = tooltip
            }
        }
    }

    static func parseOutput(_ raw: String) -> (String, String?) {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.hasPrefix("{"),
           let data = trimmed.data(using: .utf8),
           let dict = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        {
            let text = (dict["text"] as? String) ?? trimmed
            let tooltip = dict["tooltip"] as? String
            return (text, tooltip)
        }
        return (trimmed, nil)
    }
}
