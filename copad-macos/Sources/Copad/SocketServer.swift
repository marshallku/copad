import Darwin
import Foundation

/// Completion value for handlers that must return a JSON-RPC error envelope. Handlers share one
/// `(Any?) -> Void` completion, so `dispatch()` detects this type and emits the error frame.
struct RPCError: Error {
    let code: String
    let message: String
}

/// Unix socket server for coctl connections. Each client has its own thread, which blocks on a
/// semaphore while the handler runs on the main queue and calls `completion`.
final class SocketServer: @unchecked Sendable {
    private let socketPath: String
    private var serverFd: Int32 = -1

/// Called on the main thread; `completion` may fire later (e.g. after a WKWebView callback).
    var commandHandler: ((_ method: String, _ params: [String: Any], _ completion: @escaping (Any?) -> Void) -> Void)?

    var eventBus: EventBus?

    init() {
        let pid = ProcessInfo.processInfo.processIdentifier
        socketPath = "/tmp/copad-\(pid).sock"
    }

    var path: String {
        socketPath
    }

    func start() {
        unlink(socketPath)

        serverFd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard serverFd >= 0 else {
            print("[copad] socket create failed: \(String(cString: strerror(errno)))")
            return
        }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { buf in
            let count = min(socketPath.utf8.count, buf.count - 1)
            buf.copyBytes(from: socketPath.utf8.prefix(count))
            buf[count] = 0
        }

        let addrSize = socklen_t(MemoryLayout<sockaddr_un>.size)
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(serverFd, $0, addrSize)
            }
        }

        guard bound == 0 else {
            print("[copad] socket bind failed: \(String(cString: strerror(errno)))")
            close(serverFd)
            serverFd = -1
            return
        }

        listen(serverFd, 5)
        print("[copad] socket listening at \(socketPath)")

        let fd = serverFd
        Thread.detachNewThread { [weak self] in self?.acceptLoop(serverFd: fd) }
    }

    func stop() {
        if serverFd >= 0 {
            close(serverFd)
            serverFd = -1
        }
        unlink(socketPath)
    }

    // MARK: - Private

    private func acceptLoop(serverFd: Int32) {
        while true {
            let clientFd = accept(serverFd, nil, nil)
            if clientFd < 0 { break }
            Thread.detachNewThread { [weak self] in self?.handleClient(clientFd) }
        }
    }

/// 1 MiB, matching `copad-daemon::socket::read_line_capped` and the `DaemonClient` cap; bounds a
/// client that never sends a newline.
    private static let maxFrameBytes = 1024 * 1024

    private func handleClient(_ fd: Int32) {
        defer { close(fd) }

        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 4096)

        outer: while true {
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { break }
            buffer.append(contentsOf: chunk[..<n])

            while true {
                if let nlIdx = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                    let frameBytes = buffer.distance(from: buffer.startIndex, to: nlIdx) + 1
                    if frameBytes > Self.maxFrameBytes {
                        FileHandle.standardError.write(Data("[copad-socket] frame cap exceeded (\(frameBytes) > \(Self.maxFrameBytes)) — closing client\n".utf8))
                        break outer
                    }
                    let line = Data(buffer[..<nlIdx])
                    buffer = Data(buffer[buffer.index(after: nlIdx)...])

                    if let json = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                       let id = json["id"] as? String,
                       (json["method"] as? String) == "event.subscribe"
                    {
                        var resp = success(id: id, result: ["status": "subscribed"])
                        resp.append(UInt8(ascii: "\n"))
                        _ = resp.withUnsafeBytes { write(fd, $0.baseAddress!, $0.count) }
                        streamEvents(fd: fd)
                        return
                    }

                    var response = dispatch(line)
                    response.append(UInt8(ascii: "\n"))
                    _ = response.withUnsafeBytes { write(fd, $0.baseAddress!, $0.count) }
                } else {
                    if buffer.count > Self.maxFrameBytes {
                        FileHandle.standardError.write(Data("[copad-socket] partial frame exceeds cap (\(buffer.count) > \(Self.maxFrameBytes)) — closing client\n".utf8))
                        break outer
                    }
                    break // need more bytes
                }
            }
        }
    }

    private func streamEvents(fd: Int32) {
        guard let bus = eventBus else { return }
        let channel = bus.subscribe()
        defer { channel.close() }
        while let event = channel.receive() {
            let data = Data((event + "\n").utf8)
            let result = data.withUnsafeBytes { write(fd, $0.baseAddress!, $0.count) }
            if result <= 0 { break }
        }
    }

/// Access is serialized by the semaphore.
    private final class ResultBox: @unchecked Sendable {
        var value: Any?
    }

    private func dispatch(_ data: Data) -> Data {
        guard
            let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let id = json["id"] as? String,
            let method = json["method"] as? String
        else {
            return error(id: "?", code: "invalid_request", message: "malformed JSON")
        }

        let params = json["params"] as? [String: Any] ?? [:]

        let sema = DispatchSemaphore(value: 0)
        let box = ResultBox()

        DispatchQueue.main.async {
            guard let handler = self.commandHandler else {
                sema.signal()
                return
            }
            handler(method, params) { value in
                box.value = value
                sema.signal()
            }
        }

        sema.wait()

        if let err = box.value as? RPCError {
            return error(id: id, code: err.code, message: err.message)
        }
        if let result = box.value {
            return success(id: id, result: result)
        } else {
            return error(id: id, code: "unknown_method", message: "unknown: \(method)")
        }
    }

    private func success(id: String, result: Any) -> Data {
        let dict: [String: Any] = ["id": id, "ok": true, "result": result]
        return (try? JSONSerialization.data(withJSONObject: dict)) ?? Data()
    }

    private func error(id: String, code: String, message: String) -> Data {
        let dict: [String: Any] = [
            "id": id,
            "ok": false,
            "error": ["code": code, "message": message],
        ]
        return (try? JSONSerialization.data(withJSONObject: dict)) ?? Data()
    }
}
