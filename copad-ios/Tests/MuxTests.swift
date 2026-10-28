import XCTest
@testable import copad_ios
import CryptoKit
import UIKit

/// The native client's protocol layer, against the grammar comux pins in
/// `copad-mux/src/proto.rs` (`wire_grammar`) — the same strings, from the other side.
final class WireGrammarTests: XCTestCase {
    private func json(_ k: KeyEvent) -> String {
        let d = try! JSONSerialization.data(withJSONObject: k.json, options: [.sortedKeys])
        return String(decoding: d, as: UTF8.self)
    }

    func testKeysMatchCrosstermSerde() {
        XCTAssertEqual(json(KeyEvent(code: .char("a"))),
                       #"{"code":{"Char":"a"},"kind":"Press","modifiers":"","state":""}"#)
        XCTAssertEqual(json(.ctrl("b")),
                       #"{"code":{"Char":"b"},"kind":"Press","modifiers":"CONTROL","state":""}"#)
        XCTAssertEqual(json(KeyEvent(code: .enter, mods: [.shift, .alt])),
                       #"{"code":"Enter","kind":"Press","modifiers":"SHIFT | ALT","state":""}"#)
        XCTAssertEqual(json(KeyEvent(code: .f(5))),
                       #"{"code":{"F":5},"kind":"Press","modifiers":"","state":""}"#)
        XCTAssertEqual(json(KeyEvent(code: .backTab, mods: .shift)),
                       #"{"code":"BackTab","kind":"Press","modifiers":"SHIFT","state":""}"#)
        XCTAssertEqual(json(KeyEvent(code: .char("한"))),
                       #"{"code":{"Char":"한"},"kind":"Press","modifiers":"","state":""}"#)
    }

    func testCommittedTextBecomesOneKeyPerCharacter() {
        let keys = KeyEvent.keys(for: "한글 a\r\nb\t")
        XCTAssertEqual(keys.map(\.code), [.char("한"), .char("글"), .char(" "), .char("a"),
                                          .enter, .char("b"), .tab])
    }

    func testColorsAndModifiers() {
        XCTAssertNil(Palette.resolve("Reset"))
        XCTAssertEqual(Palette.resolve("#0102FF"), RGB(hex: 0x0102FF))
        XCTAssertEqual(Palette.resolve("Red"), Palette.ansi[1])
        XCTAssertEqual(Palette.resolve("DarkGray"), Palette.ansi[8])
        XCTAssertEqual(Palette.resolve("12"), Palette.ansi[12])
        XCTAssertEqual(Palette.resolve("196"), Palette.indexed(196))
        XCTAssertEqual(Mods.parse(""), [])
        XCTAssertEqual(Mods.parse("BOLD | DIM | REVERSED"), [.bold, .dim, .reversed])
    }

    func testServerMessagesParse() {
        let hello = #"{"t":"hello","mouse":true,"paste":true,"incarnation":"abc","attachment":7,"commands":true}"#
        guard case let .hello(h)? = ServerMsg.parse(Data(hello.utf8)) else { return XCTFail() }
        XCTAssertEqual(h.attachment, 7)
        XCTAssertEqual(h.incarnation, "abc")
        XCTAssertTrue(h.commands)
        guard case .unknown? = ServerMsg.parse(Data(#"{"t":"from-the-future"}"#.utf8)) else { return XCTFail() }
        XCTAssertNil(ServerMsg.parse(Data("Welcome to Ubuntu".utf8)))
    }

    func testGraphemeClustersGoAsSingleScalars() {
        let keys = KeyEvent.keys(for: "e\u{301}👨‍👩")
        XCTAssertEqual(keys.count, 5) // e, combining acute, man, ZWJ, woman
        for k in keys {
            guard case let .char(c) = k.code else { return XCTFail() }
            XCTAssertEqual(c.unicodeScalars.count, 1)
        }
    }

    func testAttachCarriesHeartbeatIncarnationAndReplace() {
        let m = ClientMsg.attach(cols: 80, rows: 24, expectIncarnation: "i1", replaces: (5, "ios:x")).json
        XCTAssertEqual(m["heartbeat"] as? Bool, true)
        XCTAssertEqual(m["expect_incarnation"] as? String, "i1")
        let r = m["replaces"] as? [String: Any]
        XCTAssertEqual(r?["attachment"] as? UInt64, 5)
        XCTAssertEqual(r?["tag"] as? String, "ios:x")
    }
}

final class GridTests: XCTestCase {
    private func frame(_ json: String) -> FrameMsg {
        guard case let .frame(f)? = ServerMsg.parse(Data(json.utf8)) else { fatalError(json) }
        return f
    }

    func testDeltasBeforeAFullFrameAreDropped() {
        var g = Grid()
        XCTAssertFalse(g.apply(frame(#"{"t":"frame","epoch":1,"cols":4,"rows":1,"full":false,"cells":[{"x":0,"y":0,"sym":"x","fg":"Reset","bg":"Reset","mods":""}],"cursor":null}"#)))
        XCTAssertTrue(g.apply(frame(#"{"t":"frame","epoch":1,"cols":4,"rows":1,"full":true,"cells":[{"x":0,"y":0,"sym":"a","fg":"Red","bg":"Reset","mods":"BOLD"}],"cursor":[1,0]}"#)))
        XCTAssertEqual(g.text(row: 0), "a   ")
        XCTAssertEqual(g[0, 0].mods, .bold)
        XCTAssertEqual(g.cursor?.x, 1)
        // A delta for another size waits for its own full frame.
        XCTAssertFalse(g.apply(frame(#"{"t":"frame","epoch":2,"cols":5,"rows":1,"full":false,"cells":[],"cursor":null}"#)))
    }

    func testAWideGlyphOwnsItsSpacerAndMovingItClearsTheOldOne() {
        var g = Grid()
        g.apply(frame(#"{"t":"frame","epoch":1,"cols":4,"rows":1,"full":true,"cells":[{"x":0,"y":0,"sym":"한","fg":"Reset","bg":"Reset","mods":""}],"cursor":null}"#))
        XCTAssertTrue(g[1, 0].skip)
        XCTAssertEqual(g.text(row: 0), "한  ")
        // The glyph moves to x=1; the wire omits spacers, so the old one must be rebuilt.
        g.apply(frame(#"{"t":"frame","epoch":1,"cols":4,"rows":1,"full":false,"cells":[{"x":0,"y":0,"sym":"a","fg":"Reset","bg":"Reset","mods":""},{"x":1,"y":0,"sym":"글","fg":"Reset","bg":"Reset","mods":""}],"cursor":null}"#))
        XCTAssertFalse(g[1, 0].skip)
        XCTAssertTrue(g[2, 0].skip)
        XCTAssertEqual(g.text(row: 0), "a글 ")
    }

    func testAWideGlyphsSpacerTakesItsBackground() {
        var g = Grid()
        g.apply(frame(##"{"t":"frame","epoch":1,"cols":4,"rows":1,"full":true,"cells":[{"x":0,"y":0,"sym":"한","fg":"Reset","bg":"#313244","mods":""},{"x":2,"y":0,"sym":"a","fg":"Reset","bg":"#313244","mods":""}],"cursor":null}"##))
        XCTAssertTrue(g[1, 0].skip)
        XCTAssertEqual(g[1, 0].bg, g[0, 0].bg, "no gap in the highlighted run")
    }

    func testCellWidths() {
        XCTAssertEqual(CellWidth.of("a"), 1)
        XCTAssertEqual(CellWidth.of("한"), 2)
        XCTAssertEqual(CellWidth.of("中"), 2)
        XCTAssertEqual(CellWidth.of("🚀"), 2)
        XCTAssertEqual(CellWidth.of("❤️"), 2)
        XCTAssertEqual(CellWidth.of("─"), 1)
    }
}

final class L0ModelTests: XCTestCase {
    private func obj(_ s: String) -> [String: Any] {
        try! JSONSerialization.jsonObject(with: Data(s.utf8)) as! [String: Any]
    }

    func testRevisionsArePerTopicAndStaleValuesIgnored() {
        var m = L0Model()
        XCTAssertEqual(m.apply(obj(#"{"t":"welcome","proto":1,"incarnation":"i","version":"1"}"#)), .welcome)
        _ = m.apply(obj(#"{"t":"state","topic":"tree","rev":5,"data":{"sessions":[],"active":"new"}}"#))
        XCTAssertEqual(m.apply(obj(#"{"t":"state","topic":"tree","rev":4,"data":{"sessions":[],"active":"old"}}"#)), .ignored)
        XCTAssertEqual(m.tree?.active, "new")
        XCTAssertEqual(m.apply(obj(#"{"t":"state","topic":"clients","rev":2,"data":{"clients":[{"id":3,"tag":"ios:x","cols":80,"rows":24,"claimed":[]}],"hidden":[]}}"#)), .topics(["clients"]))
        XCTAssertTrue(m.lists(attachment: 3, tag: "ios:x"))
        XCTAssertFalse(m.lists(attachment: 3, tag: "ios:y"))
        _ = m.apply(obj(#"{"t":"unavailable","topic":"tree","rev":6,"reason":"big"}"#))
        XCTAssertNil(m.tree)
    }

    func testElapsedLabels() {
        let now = Date(timeIntervalSince1970: 10_000)
        XCTAssertEqual(elapsedLabel(since: 9_990, now: now), "", "nothing under a minute, like the TUI")
        XCTAssertEqual(elapsedLabel(since: 9_700, now: now), "5m")
        XCTAssertEqual(elapsedLabel(since: 2_000, now: now), "2h13m")
    }
}

final class TransportTests: XCTestCase {
    func testLinesSplitAcrossChunks() {
        var s = LineSplitter()
        XCTAssertEqual(s.feed(Data("ab".utf8)), [])
        XCTAssertEqual(s.feed(Data("c\nde\n\nf".utf8)).map { String(decoding: $0, as: UTF8.self) }, ["abc", "de"])
        XCTAssertEqual(s.feed(Data("\n".utf8)).map { String(decoding: $0, as: UTF8.self) }, ["f"])
    }

    func testFingerprintMatchesSshKeygen() {
        // `ssh-keygen -lf` printed SHA256:raaezL8g4BFJTV5fOqiqJO1nrEEWxW2a9WybNtnkF7M for this key.
        XCTAssertEqual(sshFingerprint("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOOMeekBOvAN2GkSmBltqyT/dJAvHkKEGtbigjJfE4xc"), "SHA256:raaezL8g4BFJTV5fOqiqJO1nrEEWxW2a9WybNtnkF7M")
    }
}

/// The keyboard contract (#122, codex C2): marked text is never sent; a commit sends it once;
/// settling before navigation commits it FIRST.
@MainActor
final class InputProxyTests: XCTestCase {
    final class Recorder: KeySink {
        var out: [String] = []
        func sendText(_ text: String) { out.append("text:\(text)") }
        func sendKey(_ k: KeyEvent) { out.append("key:\(k.json["code"]!)/\(k.modsString)") }
        func send(_ msg: ClientMsg) { out.append("msg") }
    }

    func testMarkedTextWaitsForTheCommit() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        p.setMarkedText("하", selectedRange: NSRange(location: 1, length: 0))
        p.textViewDidChange(p)
        p.setMarkedText("한", selectedRange: NSRange(location: 1, length: 0))
        p.textViewDidChange(p)
        XCTAssertEqual(r.out, [], "nothing leaves while the syllable is being composed")
        p.commitComposition()
        XCTAssertEqual(r.out, ["text:한"])
        p.commitComposition()
        XCTAssertEqual(r.out, ["text:한"], "a commit is sent once")
    }

    func testEnterCommitsThePendingSyllableFirst() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        p.setMarkedText("글", selectedRange: NSRange(location: 1, length: 0))
        XCTAssertFalse(p.textView(p, shouldChangeTextIn: NSRange(location: 1, length: 0), replacementText: "\n"))
        XCTAssertEqual(r.out, ["text:글", "key:Enter/"])
    }

    /// The iPhone's Korean keyboard (no marked text): it replaces the last character in place.
    func testTheKoreanKeyboardsInPlaceRewritesSendEachSyllableOnce() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        func type(_ c: String) { p.insertText(c); p.textViewDidChange(p) }
        func rewriteLast(_ c: String) {
            let end = p.endOfDocument
            p.replace(p.textRange(from: p.position(from: end, offset: -1)!, to: end)!, withText: c)
            p.textViewDidChange(p)
        }
        type("ㅎ"); rewriteLast("하"); rewriteLast("한")
        XCTAssertEqual(r.out, [], "the syllable is still open")
        type("ㄱ")
        XCTAssertEqual(r.out, ["text:한"])
        rewriteLast("그"); rewriteLast("글")
        // 글 + ㅏ moves the final consonant: 그 + 라.
        rewriteLast("그라")
        XCTAssertEqual(r.out, ["text:한", "text:그"])
        type(" ")
        XCTAssertEqual(r.out, ["text:한", "text:그", "text:라 "])
        type("ㅋ")
        p.commitComposition()
        XCTAssertEqual(r.out.last, "text:ㅋ", "a commit sends the held character")
    }

    /// Deleting past the held syllable: the keyboard remembers what it composed and rewrites the
    /// syllable before (한 → 하) in the view, which must erase it in the pane.
    func testDeletingIntoASentSyllableErasesItInThePane() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        func rewriteLast(_ c: String) {
            let end = p.endOfDocument
            p.replace(p.textRange(from: p.position(from: end, offset: -1)!, to: end)!, withText: c)
            p.textViewDidChange(p)
        }
        p.insertText("한"); p.textViewDidChange(p)
        p.insertText("ㄱ"); p.textViewDidChange(p)
        rewriteLast("그"); rewriteLast("글")
        XCTAssertEqual(r.out, ["text:한"])
        rewriteLast("그"); rewriteLast("ㄱ")
        p.deleteBackward(); p.textViewDidChange(p)
        XCTAssertEqual(r.out, ["text:한"], "only the held syllable went")
        rewriteLast("하")
        XCTAssertEqual(r.out, ["text:한", "key:Backspace/"])
        rewriteLast("ㅎ")
        p.deleteBackward(); p.textViewDidChange(p)
        p.deleteBackward()
        XCTAssertEqual(r.out, ["text:한", "key:Backspace/", "key:Backspace/"])
    }

    func testDeletingASpaceLeavesTheSyllableBeforeIt() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        p.insertText("한"); p.textViewDidChange(p)
        p.insertText(" "); p.textViewDidChange(p)
        p.deleteBackward(); p.textViewDidChange(p)
        XCTAssertEqual(r.out, ["text:한 ", "key:Backspace/"])
    }

    func testDeletingSentLatinTextErasesItInThePane() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        p.insertText("ab"); p.textViewDidChange(p)
        p.deleteBackward(); p.textViewDidChange(p)
        p.insertText("c"); p.textViewDidChange(p)
        XCTAssertEqual(r.out, ["text:ab", "key:Backspace/", "text:c"])
    }

    func testArmingCtrlSendsTheHeldSyllableFirst() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        p.insertText("한"); p.textViewDidChange(p)
        p.ctrlArmed = true
        p.insertText("c"); p.textViewDidChange(p)
        XCTAssertEqual(r.out, ["text:한", "key:[\"Char\": \"c\"]/CONTROL"])
    }

    func testBackspaceOnAnEmptyFieldIsThePanes() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        p.deleteBackward()
        XCTAssertEqual(r.out, ["key:Backspace/"])
    }

    func testStickyCtrlAppliesToTheNextCharacterOnly() {
        let p = InputProxy(), r = Recorder()
        p.session = r
        p.ctrlArmed = true
        p.insertText("cx")
        p.textViewDidChange(p)
        XCTAssertEqual(r.out.first, "key:[\"Char\": \"c\"]/CONTROL")
        XCTAssertEqual(r.out.last, "text:x")
        XCTAssertFalse(p.ctrlArmed)
    }
}

/// The device key (decisions #173). The Simulator has no Secure Enclave, so the ECDSA path an
/// enclave key takes is exercised with an imported software P-256 key; the Ed25519 path is
/// what a Simulator (or an older install) uses on its own.
final class DeviceKeyTests: XCTestCase {
    override func tearDown() {
        UserDefaults.standard.removeObject(forKey: "copad.ssh.kind")
    }

    func testAnImportedP256KeyIsOfferedAsECDSA() throws {
        let k = P256.Signing.PrivateKey()
        try DeviceKey.importP256(base64: k.rawRepresentation.base64EncodedString())
        XCTAssertEqual(DeviceKey.kind, .p256)
        let line = try DeviceKey.publicLine()
        XCTAssertTrue(line.hasPrefix("ecdsa-sha2-nistp256 "), line)
        // RFC 5656: string "ecdsa-sha2-nistp256", string "nistp256", string Q (uncompressed).
        func str(_ d: Data) -> Data { withUnsafeBytes(of: UInt32(d.count).bigEndian) { Data($0) } + d }
        let blob = str(Data("ecdsa-sha2-nistp256".utf8)) + str(Data("nistp256".utf8))
            + str(k.publicKey.x963Representation)
        XCTAssertEqual(line, "ecdsa-sha2-nistp256 \(blob.base64EncodedString()) copad-ios")
    }

    func testAnEd25519SeedRoundTrips() throws {
        let k = Curve25519.Signing.PrivateKey()
        try DeviceKey.importSeed(base64: k.rawRepresentation.base64EncodedString())
        XCTAssertEqual(DeviceKey.kind, .ed25519)
        XCTAssertTrue(try DeviceKey.publicLine().hasPrefix("ssh-ed25519 "))
    }

    /// The Simulator on Apple silicon has an enclave, so the real path runs here too.
    func testANewEnclaveKeyIsOfferedAsECDSA() throws {
        try XCTSkipUnless(DeviceKey.secureEnclaveAvailable, "no Secure Enclave")
        try DeviceKey.useNewSecureEnclaveKey()
        XCTAssertEqual(DeviceKey.kind, .secureEnclave)
        let first = try DeviceKey.publicLine()
        XCTAssertTrue(first.hasPrefix("ecdsa-sha2-nistp256 "), first)
        XCTAssertEqual(try DeviceKey.publicLine(), first, "the enclave key is reloaded, not regenerated")
        try DeviceKey.useNewSecureEnclaveKey()
        XCTAssertNotEqual(try DeviceKey.publicLine(), first, "a new key replaces it")
    }

    func testABadKeyIsRefused() {
        XCTAssertThrowsError(try DeviceKey.importP256(base64: "AAAA"))
    }
}

/// The bundled Nerd Font (decisions #174): it loads, and it carries the glyphs prompts and
/// TUIs draw — powerline separators and Nerd icons — so they are not tofu.
final class TerminalFontTests: XCTestCase {
    func testTheBundledNerdFontLoadsWithItsIcons() throws {
        for name in ["JetBrainsMonoNFM-Regular", "JetBrainsMonoNFM-Bold"] {
            let f = try XCTUnwrap(UIFont(name: name, size: 13), "\(name) is not registered")
            let ct = f as CTFont
            // U+E0B0 powerline arrow, U+F121 nf-fa-code, U+F0001 (nf-md, a surrogate pair).
            for scalar in [0xE0B0, 0xF121, 0xF0001] {
                var chars = Array(String(UnicodeScalar(scalar)!).utf16)
                var glyphs = [CGGlyph](repeating: 0, count: chars.count)
                XCTAssertTrue(CTFontGetGlyphsForCharacters(ct, &chars, &glyphs, chars.count),
                              "\(name) lacks U+\(String(scalar, radix: 16, uppercase: true))")
            }
        }
    }
}
