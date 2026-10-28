import XCTest
import NIOCore
import NIOEmbedded
import NIOSSH
@testable import copad_ios

/// Password logins (the App Store review account, and hosts that allow passwords).
final class HostAuthTests: XCTestCase {
    /// A host list saved before `auth` existed must still load: a decode failure empties the list.
    func testHostsSavedBeforePasswordsStillDecode() throws {
        let old = #"[{"id":"6F9619FF-8B86-D011-B42D-00C04FC964FF","name":"mac","host":"box","port":22,"user":"me","command":"exec comux proxy --stdio"}]"#
        let hosts = try JSONDecoder().decode([MuxHost].self, from: Data(old.utf8))
        XCTAssertEqual(hosts.count, 1)
        XCTAssertNil(hosts[0].auth)
        XCTAssertFalse(hosts[0].usesPassword)
    }

    func testAPasswordHostRoundTrips() throws {
        var h = MuxHost(name: "demo", host: "demo.example", user: "review")
        h.auth = .password
        let back = try JSONDecoder().decode(MuxHost.self, from: JSONEncoder().encode(h))
        XCTAssertTrue(back.usesPassword)
        XCTAssertFalse(String(decoding: try JSONEncoder().encode(h), as: UTF8.self).contains("hunter2"))
    }

    func testThePasswordLivesInTheKeychainUnderTheHost() throws {
        let id = UUID()
        defer { HostPassword.delete(id) }
        XCTAssertNil(HostPassword.read(id))
        try HostPassword.save("first", for: id)
        try HostPassword.save("second", for: id)
        XCTAssertEqual(HostPassword.read(id), "second", "saving again replaces")
        HostPassword.delete(id)
        XCTAssertNil(HostPassword.read(id))
    }

    private func offer(_ c: SSHCredential, _ methods: NIOSSHAvailableUserAuthenticationMethods,
                       _ auth: UserAuth? = nil) throws -> (UserAuth, NIOSSHUserAuthenticationOffer?) {
        let a = auth ?? UserAuth(user: "review", credential: c)
        let loop = EmbeddedEventLoop()
        let p = loop.makePromise(of: NIOSSHUserAuthenticationOffer?.self)
        a.nextAuthenticationType(availableMethods: methods, nextChallengePromise: p)
        return (a, try p.futureResult.wait())
    }

    func testAPasswordIsOfferedOnceWhenTheServerTakesPasswords() throws {
        let (a, first) = try offer(.password("pw"), [.publicKey, .password])
        guard case let .password(pw)? = first?.offer else { return XCTFail("no password offer") }
        XCTAssertEqual(pw.password, "pw")
        XCTAssertEqual(first?.username, "review")
        let (_, again) = try offer(.password("pw"), [.password], a)
        XCTAssertNil(again, "a rejected password is not offered again")
        XCTAssertTrue(a.exhausted)
        XCTAssertTrue(a.isPassword)
    }

    func testAPasswordIsNotOfferedToAKeyOnlyServer() throws {
        let (a, o) = try offer(.password("pw"), [.publicKey])
        XCTAssertNil(o)
        XCTAssertTrue(a.exhausted)
    }

    func testAKeyIsOfferedAsBefore() throws {
        let key = NIOSSHPrivateKey(ed25519Key: .init())
        let (a, o) = try offer(.key(key), [.publicKey, .password])
        guard case .privateKey? = o?.offer else { return XCTFail("no key offer") }
        XCTAssertFalse(a.isPassword)
    }
}
