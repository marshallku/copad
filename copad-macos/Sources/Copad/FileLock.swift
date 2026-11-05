import Darwin
import Foundation

/// `lockf` rather than `flock`: Swift's Darwin module exports `struct flock`, which shadows the
/// `flock(_:_:)` function and breaks the call site.
final class FileLock {
    private let fd: Int32
    private(set) var holding = false

    init(path: URL) throws {
        let cstr = path.path(percentEncoded: false)
        let opened = open(cstr, O_CREAT | O_RDWR, 0o600)
        if opened < 0 {
            let err = String(cString: strerror(errno))
            throw FileLockError.openFailed(path: cstr, message: err)
        }
        fd = opened
    }

    deinit {
        if holding { _ = Darwin.lockf(fd, F_ULOCK, 0) }
        Darwin.close(fd)
    }

    func tryAcquire() throws -> Bool {
        let rc = Darwin.lockf(fd, F_TLOCK, 0)
        if rc == 0 {
            holding = true
            return true
        }
        if errno == EAGAIN || errno == EACCES {
            return false
        }
        throw FileLockError.lockfFailed(message: String(cString: strerror(errno)))
    }

    func release() {
        guard holding else { return }
        _ = Darwin.lockf(fd, F_ULOCK, 0)
        holding = false
    }
}

enum FileLockError: Error, CustomStringConvertible {
    case openFailed(path: String, message: String)
    case lockfFailed(message: String)

    var description: String {
        switch self {
        case let .openFailed(path, message): "FileLock open(\(path)): \(message)"
        case let .lockfFailed(message): "FileLock lockf: \(message)"
        }
    }
}
