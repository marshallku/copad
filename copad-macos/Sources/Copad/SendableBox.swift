import Foundation

/// Bridge for a non-`Sendable` value (usually a closure) that is invoked on one known thread
/// right after hopping actors, so the race the compiler warns about cannot occur.
final class SendableBox<T>: @unchecked Sendable {
    let value: T
    init(_ value: T) {
        self.value = value
    }
}

extension NSLock {
    @discardableResult
    func withLock<T>(_ body: () -> T) -> T {
        lock(); defer { unlock() }
        return body()
    }
}
