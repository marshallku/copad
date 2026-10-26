import CryptoKit
import XCTest
@testable import copad_ios

/// The end-to-end push envelope (decisions #177): the same bytes comux's push tests open.
final class PushEnvelopeTests: XCTestCase {
    private let key = SymmetricKey(data: Data(0..<32))
    private let vector = "ZGVmZ2hpamtsbW5vMzm8CR2QdKQciexQMcnS327gdmvlCdFIhb/DJpjGiH+2xWK0JWb3Yd/JWnyRI7+rfnv+jk4jMkCflY6rYoNG4P6NsvSrWFyi8wUC4g=="

    func testTheSharedVectorOpens() {
        let opened = PushEnvelope.open(sealed: vector, host: "host-1", key: key)
        XCTAssertEqual(opened, .init(host: "host-1", title: "Claude finished", body: "본문", pane: "nonce-7"))
    }

    func testNothingOpensWithAnotherHostOrKeyOrTamperedBytes() {
        XCTAssertNil(PushEnvelope.open(sealed: vector, host: "host-2", key: key), "bound to the host id")
        XCTAssertNil(PushEnvelope.open(sealed: vector, host: "host-1", key: SymmetricKey(size: .bits256)))
        var raw = Data(base64Encoded: vector)!
        raw[20] ^= 1
        XCTAssertNil(PushEnvelope.open(sealed: raw.base64EncodedString(), host: "host-1", key: key))
        XCTAssertNil(PushEnvelope.open(sealed: "not base64", host: "host-1", key: key))
    }

    func testRoutingComesOnlyFromTheEnvelope() {
        let keys = ["host-1": key]
        // Plain routing a relay could add is ignored; a missing key opens nothing.
        let forged: [AnyHashable: Any] = ["comux": ["host": "host-1", "pane": "evil"], "kid": "host-1"]
        XCTAssertNil(PushEnvelope.open(forged, key: { keys[$0] }))
        let real: [AnyHashable: Any] = ["sealed": vector, "kid": "host-1", "comux": ["pane": "evil"]]
        XCTAssertEqual(PushEnvelope.open(real, key: { keys[$0] })?.pane, "nonce-7")
        XCTAssertNil(PushEnvelope.open(real, key: { _ in nil }))
    }

    func testRegistrationNeedsAnEncryptingComux() {
        XCTAssertNil(PushRegistry.registrationProblem(["ok": true, "e2e": 1]))
        XCTAssertEqual(PushRegistry.registrationProblem(["ok": true]), "update comux on this host to get pushes")
        XCTAssertEqual(PushRegistry.registrationProblem(["ok": false, "error": "pushes are off"]), "pushes are off")
    }

    func testAKeyIsMadeOnceAndRemovedWithTheHost() throws {
        let host = "test-\(UUID().uuidString)"
        defer { PushKeys.remove(host: host) }
        let first = try PushKeys.ensure(host: host)
        XCTAssertEqual(Data(base64Encoded: first)?.count, 32)
        XCTAssertEqual(try PushKeys.ensure(host: host), first, "stable per host")
        PushKeys.remove(host: host)
        XCTAssertNil(PushKeys.key(host: host))
    }
}
