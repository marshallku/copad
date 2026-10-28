import CryptoKit
import Foundation
import NIOCore
import NIOPosix
import NIOSSH

/// Why an SSH connection did not come up, in words the host screen can show.
enum SSHError: LocalizedError {
    /// First connection to this host: the user must confirm the key (TOFU).
    case unknownHostKey(String)
    /// The host presents a different key than the one confirmed before. Never auto-accepted.
    case changedHostKey(expected: String, got: String)
    case authFailed(password: Bool)
    case execFailed(String)
    case closed(String)

    var errorDescription: String? {
        switch self {
        case .unknownHostKey: return "Unknown host key — confirm it to continue."
        case .changedHostKey: return "The host key CHANGED. Refusing to connect."
        case .authFailed(password: false): return "Authentication failed — is this device's key in authorized_keys?"
        case .authFailed(password: true): return "Authentication failed — check the user and password, and that the host allows password logins."
        case let .execFailed(m): return "Could not start comux on the host: \(m)"
        case let .closed(m): return m
        }
    }
}

/// An SSH client connection; each `exec` opens one channel (one `comux proxy --stdio`).
final class SSHConnection {
    static let setupTimeout = TimeAmount.seconds(20)
    private let group: MultiThreadedEventLoopGroup
    private let channel: Channel
    private let hostCheck: HostKeyCheck
    private let auth: UserAuth

    private init(group: MultiThreadedEventLoopGroup, channel: Channel,
                 hostCheck: HostKeyCheck, auth: UserAuth) {
        self.group = group
        self.channel = channel
        self.hostCheck = hostCheck
        self.auth = auth
    }

    /// Connect and authenticate with `credential`. `knownKey` is the OpenSSH host key the user
    /// confirmed earlier (nil = first contact: the connection fails with `unknownHostKey`
    /// carrying the key to confirm).
    static func connect(host: String, port: Int, user: String,
                        credential: SSHCredential, knownKey: String?) async throws -> SSHConnection {
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        let hostCheck = HostKeyCheck(known: knownKey)
        let auth = UserAuth(user: user, credential: credential)
        let bootstrap = ClientBootstrap(group: group)
            .connectTimeout(.seconds(10))
            .channelOption(ChannelOptions.socketOption(.so_keepalive), value: 1)
            .channelInitializer { ch in
                ch.eventLoop.makeCompletedFuture {
                    let ssh = NIOSSHHandler(
                        role: .client(.init(userAuthDelegate: auth, serverAuthDelegate: hostCheck)),
                        allocator: ch.allocator, inboundChildChannelInitializer: nil)
                    try ch.pipeline.syncOperations.addHandler(ssh)
                    try ch.pipeline.syncOperations.addHandler(CloseOnError())
                }
            }
        do {
            let ch = try await bootstrap.connect(host: host, port: port).get()
            // Authentication completes asynchronously; a session channel opens only after it,
            // so the first exec doubles as the auth check. A rejected host key surfaces there.
            return SSHConnection(group: group, channel: ch, hostCheck: hostCheck, auth: auth)
        } catch {
            try? await group.shutdownGracefully()
            if let e = hostCheck.failure { throw e }
            throw error
        }
    }

    /// Run `command` on a new session channel. Bytes from its stdout go to `onData`; its end
    /// (with the reason, stderr's tail included) to `onClose`, exactly once.
    func exec(_ command: String,
              onData: @escaping (Data) -> Void,
              onClose: @escaping (String) -> Void) async throws -> ExecChannel {
        let promise = channel.eventLoop.makePromise(of: Channel.self)
        let handler = ExecHandler(command: command, onData: onData, onClose: onClose)
        // Key exchange and auth happen before the first channel opens and have no deadline of
        // their own: a host that accepts the TCP connection and then stalls would leave the
        // app on "Connecting…" for good.
        let deadline = channel.eventLoop.scheduleTask(in: Self.setupTimeout) { [channel] in
            promise.fail(SSHError.closed("The host accepted the connection but SSH did not finish setting up within \(Self.setupTimeout.nanoseconds / 1_000_000_000) s."))
            channel.close(promise: nil)
        }
        promise.futureResult.whenComplete { _ in deadline.cancel() }
        channel.eventLoop.execute {
            do {
                let ssh = try self.channel.pipeline.syncOperations.handler(type: NIOSSHHandler.self)
                ssh.createChannel(promise, channelType: .session) { child, type in
                    guard type == .session else {
                        return child.eventLoop.makeFailedFuture(SSHError.execFailed("not a session channel"))
                    }
                    return child.eventLoop.makeCompletedFuture {
                        try child.pipeline.syncOperations.addHandler(handler)
                    }
                }
            } catch {
                promise.fail(error)
            }
        }
        do {
            let child = try await promise.futureResult.get()
            return ExecChannel(channel: child)
        } catch {
            // Key exchange and auth run before the first channel can open, so their failures
            // land here; name them instead of a bare "channel closed".
            if let f = hostCheck.failure { throw f }
            if auth.exhausted { throw SSHError.authFailed(password: auth.isPassword) }
            if let e = error as? SSHError { throw e }
            throw SSHError.execFailed("\(error)")
        }
    }

    func close() {
        channel.close(promise: nil)
        group.shutdownGracefully { _ in }
    }
}

/// Write side of an exec channel.
final class ExecChannel {
    private let channel: Channel

    init(channel: Channel) { self.channel = channel }

    func write(_ data: Data) {
        var buf = channel.allocator.buffer(capacity: data.count)
        buf.writeBytes(data)
        channel.writeAndFlush(buf, promise: nil)
    }

    func close() { channel.close(promise: nil) }
}

/// One exec channel: sends the exec request, forwards stdout, keeps stderr's tail for the
/// close reason.
private final class ExecHandler: ChannelDuplexHandler {
    typealias InboundIn = SSHChannelData
    typealias InboundOut = ByteBuffer
    typealias OutboundIn = ByteBuffer
    typealias OutboundOut = SSHChannelData

    private let command: String
    private let onData: (Data) -> Void
    private let onClose: (String) -> Void
    private var stderrTail = ""
    private var exitStatus: Int?
    private var closed = false

    init(command: String, onData: @escaping (Data) -> Void, onClose: @escaping (String) -> Void) {
        self.command = command
        self.onData = onData
        self.onClose = onClose
    }

    func handlerAdded(context: ChannelHandlerContext) {
        context.channel.setOption(ChannelOptions.allowRemoteHalfClosure, value: true).whenFailure { _ in }
    }

    func channelActive(context: ChannelHandlerContext) {
        let req = SSHChannelRequestEvent.ExecRequest(command: command, wantReply: true)
        context.triggerUserOutboundEvent(req).whenFailure { [weak self] err in
            self?.finish("exec refused: \(err)")
            context.close(promise: nil)
        }
        context.fireChannelActive()
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let msg = unwrapInboundIn(data)
        guard case let .byteBuffer(buf) = msg.data else { return }
        let bytes = Data(buf.readableBytesView)
        switch msg.type {
        case .channel: onData(bytes)
        case .stdErr:
            stderrTail += String(decoding: bytes, as: UTF8.self)
            if stderrTail.count > 400 { stderrTail = String(stderrTail.suffix(400)) }
        default: break
        }
    }

    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        if let e = event as? SSHChannelRequestEvent.ExitStatus { exitStatus = e.exitStatus }
        if event is ChannelEvent { context.close(promise: nil) } // remote EOF
        context.fireUserInboundEventTriggered(event)
    }

    func channelInactive(context: ChannelHandlerContext) {
        var why = "connection closed"
        if let s = exitStatus { why += " (exit \(s))" }
        let err = stderrTail.trimmingCharacters(in: .whitespacesAndNewlines)
        if !err.isEmpty { why += " — \(err)" }
        finish(why)
        context.fireChannelInactive()
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        finish("ssh error: \(error)")
        context.close(promise: nil)
    }

    func write(context: ChannelHandlerContext, data: NIOAny, promise: EventLoopPromise<Void>?) {
        let buf = unwrapOutboundIn(data)
        context.write(wrapOutboundOut(SSHChannelData(type: .channel, data: .byteBuffer(buf))), promise: promise)
    }

    private func finish(_ why: String) {
        guard !closed else { return }
        closed = true
        onClose(why)
    }
}

enum SSHCredential {
    case key(NIOSSHPrivateKey)
    case password(String)
}

/// User auth: offers the one credential once, if the server accepts that method.
final class UserAuth: NIOSSHClientUserAuthenticationDelegate {
    private let user: String
    private let credential: SSHCredential
    private var offered = false
    /// Nothing (more) to offer: the credential was rejected, or the server takes no method we have.
    private(set) var exhausted = false

    init(user: String, credential: SSHCredential) { self.user = user; self.credential = credential }

    var isPassword: Bool { if case .password = credential { true } else { false } }

    func nextAuthenticationType(availableMethods: NIOSSHAvailableUserAuthenticationMethods,
                                nextChallengePromise: EventLoopPromise<NIOSSHUserAuthenticationOffer?>) {
        let offer: NIOSSHUserAuthenticationOffer.Offer?
        switch credential {
        case let .key(k) where availableMethods.contains(.publicKey):
            offer = .privateKey(.init(privateKey: k))
        case let .password(p) where availableMethods.contains(.password):
            offer = .password(.init(password: p))
        default:
            offer = nil
        }
        guard !offered, let offer else {
            exhausted = true
            nextChallengePromise.succeed(nil) // nothing else to offer: auth fails
            return
        }
        offered = true
        nextChallengePromise.succeed(NIOSSHUserAuthenticationOffer(username: user, serviceName: "", offer: offer))
    }
}

/// NIOSSH reports a refused host key (and other connection errors) down the pipeline but leaves
/// the connection open, so the channel the first exec is waiting for never opens or fails. Closing
/// fails it at once and the caller reads the reason off `HostKeyCheck` / `UserAuth`.
final class CloseOnError: ChannelInboundHandler {
    typealias InboundIn = Any

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        context.close(promise: nil)
    }
}

/// Trust on first use: an unknown key fails with `unknownHostKey(key)` so the UI can ask; a
/// key different from the confirmed one fails with `changedHostKey` and is never accepted.
final class HostKeyCheck: NIOSSHClientServerAuthenticationDelegate {
    private let known: String?
    private(set) var failure: SSHError?

    init(known: String?) { self.known = known }

    func validateHostKey(hostKey: NIOSSHPublicKey, validationCompletePromise: EventLoopPromise<Void>) {
        let got = String(openSSHPublicKey: hostKey)
        switch known {
        case nil:
            failure = .unknownHostKey(got)
            validationCompletePromise.fail(failure!)
        case let k? where k == got:
            validationCompletePromise.succeed(())
        case let k?:
            failure = .changedHostKey(expected: k, got: got)
            validationCompletePromise.fail(failure!)
        }
    }
}

/// SHA256 fingerprint of an OpenSSH public key line, as `ssh-keygen -l` prints it.
func sshFingerprint(_ openSSH: String) -> String {
    let parts = openSSH.split(separator: " ")
    guard parts.count >= 2, let blob = Data(base64Encoded: String(parts[1])) else { return openSSH }
    let digest = SHA256.hash(data: blob)
    var b64 = Data(digest).base64EncodedString()
    while b64.hasSuffix("=") { b64.removeLast() }
    return "SHA256:\(b64)"
}
