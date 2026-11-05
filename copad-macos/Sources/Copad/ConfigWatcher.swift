import Foundation

/// Watches the config file via kqueue. Editors save by writing a temp file and renaming it over
/// the original, so rename/delete events reopen the file and install a fresh source. Retries
/// every 2 s while the file doesn't exist yet.
///
/// All methods MUST be called on the main queue; that single-queue invariant is what makes
/// `@unchecked Sendable` safe.
final class ConfigWatcher: @unchecked Sendable {
    private nonisolated(unsafe) var source: (any DispatchSourceFileSystemObject)?
    private nonisolated(unsafe) var debounceWork: DispatchWorkItem?
    private nonisolated(unsafe) var retryWork: DispatchWorkItem?

    var onChange: (() -> Void)?

    private let url: URL

    init(url: URL) {
        // Watch the target, not the symlink inode (dotfile managers symlink ~/.config).
        self.url = url.resolvingSymlinksInPath()
    }

    // MARK: - Public

    func start() {
        openAndWatch()
    }

    func stop() {
        retryWork?.cancel()
        retryWork = nil
        debounceWork?.cancel()
        debounceWork = nil
        source?.cancel()
        source = nil
    }

    // MARK: - Private

    private func openAndWatch() {
        let fd = open(url.path, O_EVTONLY)
        guard fd != -1 else {
            scheduleRetry()
            return
        }

        retryWork?.cancel()
        retryWork = nil

        let src = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: fd,
            eventMask: [.write, .rename, .delete],
            queue: .main,
        )

        src.setEventHandler { [weak self, weak src] in
            guard let self else { return }
            let flags = src?.data ?? []
            scheduleReload()
            if flags.contains(.rename) || flags.contains(.delete) {
                restartAfterDelay()
            }
        }

        src.setCancelHandler { [fd] in
            close(fd)
        }

        source = src
        src.resume()
    }

    private func scheduleReload() {
        debounceWork?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.onChange?() }
        debounceWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2, execute: work)
    }

    private func restartAfterDelay() {
        // Cancel only the source (the fd is stale). Keep debounceWork: the content is already
        // final when kqueue fires, and reopening after the debounce avoids fighting the
        // editor's atomic-write sequence.
        source?.cancel()
        source = nil
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
            self?.openAndWatch()
        }
    }

    private func scheduleRetry() {
        retryWork?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.openAndWatch() }
        retryWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.0, execute: work)
    }
}
