import XCTest
@testable import CopadCore

final class ComuxGraphBridgeTests: XCTestCase {
    func testMessagesParseOnlyForTheCurrentConnection() {
        XCTAssertEqual(ComuxGraphMessage.parse(["type": "ready"], epoch: "k#1"), .ready)
        XCTAssertEqual(ComuxGraphMessage.parse(["type": "jump", "token": "a1-2", "epoch": "k#1"], epoch: "k#1"),
                       .jump(token: "a1-2"))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "jump", "token": "a1-2", "epoch": "k#0"], epoch: "k#1"))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "jump", "token": "a1-2"], epoch: "k#1"))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "nope", "epoch": "k#1"], epoch: "k#1"))
        XCTAssertNil(ComuxGraphMessage.parse("jump", epoch: "k#1"))
    }

    func testFieldsAreBounded() {
        let e = "k#1"
        XCTAssertNil(ComuxGraphMessage.parse(["type": "jump", "token": "bad token", "epoch": e], epoch: e))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "jump", "token": String(repeating: "a", count: 129), "epoch": e], epoch: e))
        XCTAssertEqual(ComuxGraphMessage.parse(["type": "history", "token": "t", "before": NSNull(), "epoch": e], epoch: e),
                       .history(token: "t", before: nil, limit: nil))
        XCTAssertEqual(ComuxGraphMessage.parse(["type": "history", "token": "t", "before": 12, "limit": 1, "epoch": e], epoch: e),
                       .history(token: "t", before: 12, limit: 1))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "history", "token": "t", "before": -1, "epoch": e], epoch: e))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "history", "token": "t", "limit": 5000, "epoch": e], epoch: e))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "history", "token": "t", "before": 1.5, "epoch": e], epoch: e))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "history", "token": "t", "before": true, "epoch": e], epoch: e))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "open", "path": "relative/x", "epoch": e], epoch: e))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "open", "path": "/a\nb", "epoch": e], epoch: e))
        XCTAssertEqual(ComuxGraphMessage.parse(["type": "mode", "mode": "all", "epoch": e], epoch: e), .mode("all"))
        XCTAssertNil(ComuxGraphMessage.parse(["type": "mode", "mode": "x", "epoch": e], epoch: e))
    }

    func testOnlyARegularFileOpens() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("graph-open-\(getpid())")
        try? FileManager.default.removeItem(at: dir)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent("a.txt")
        try "x".write(to: file, atomically: true, encoding: .utf8)
        let link = dir.appendingPathComponent("link")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: file)
        let dirLink = dir.appendingPathComponent("dirlink")
        try FileManager.default.createSymbolicLink(at: dirLink, withDestinationURL: dir)
        XCTAssertNotNil(ComuxGraphMessage.openableFile(file.path))
        XCTAssertNotNil(ComuxGraphMessage.openableFile(link.path), "a link to a regular file opens its target")
        XCTAssertNil(ComuxGraphMessage.openableFile(dir.path))
        XCTAssertNil(ComuxGraphMessage.openableFile(dirLink.path))
        XCTAssertNil(ComuxGraphMessage.openableFile(dir.appendingPathComponent("missing").path))
    }
}
