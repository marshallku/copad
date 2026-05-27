import Darwin
import Foundation

enum UnixSocket {
    /// Returns a connected SOCK_STREAM fd, or nil on any failure (path too
    /// long, socket()/connect() error). Caller owns the fd and must close().
    static func connect(path: String) -> Int32? {
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let cstr = path.utf8CString
        if cstr.count > MemoryLayout.size(ofValue: addr.sun_path) {
            Darwin.close(fd)
            return nil
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { p in
            p.withMemoryRebound(to: CChar.self, capacity: cstr.count) { dst in
                _ = cstr.withUnsafeBufferPointer { src in
                    memcpy(dst, src.baseAddress, cstr.count)
                }
            }
        }
        let rc = withUnsafePointer(to: &addr) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                Darwin.connect(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if rc != 0 {
            Darwin.close(fd)
            return nil
        }
        return fd
    }
}
