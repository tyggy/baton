import CryptoKit
import Foundation
import XCTest

@testable import PushCrypto

/// Vectors shared with the Rust daemon: crates/baton-daemon/testdata/push_vectors.json.
private func loadVectors() throws -> [String: Any] {
    let url = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // PushCryptoTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // PushCrypto
        .deletingLastPathComponent()  // apple
        .deletingLastPathComponent()  // repo root
        .appendingPathComponent("crates/baton-daemon/testdata/push_vectors.json")
    let data = try Data(contentsOf: url)
    return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
}

private func hex(_ s: String) -> Data {
    var out = Data(capacity: s.count / 2)
    var i = s.startIndex
    while i < s.endIndex {
        let j = s.index(i, offsetBy: 2)
        out.append(UInt8(s[i..<j], radix: 16)!)
        i = j
    }
    return out
}

final class PushCryptoTests: XCTestCase {
    /// RFC 9180 A.2.1 through CryptoKit: proves CryptoKit's suite is the
    /// same construction the Rust `hpke` crate implements.
    func testRFC9180KnownAnswer() throws {
        let v = try XCTUnwrap(try loadVectors()["rfc9180_a2_1"] as? [String: String])
        let sk = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: hex(v["skRm"]!))
        XCTAssertEqual(sk.publicKey.rawRepresentation, hex(v["pkRm"]!))
        var r = try HPKE.Recipient(
            privateKey: sk, ciphersuite: .Curve25519_SHA256_ChachaPoly,
            info: hex(v["info"]!), encapsulatedKey: hex(v["enc"]!))
        let pt = try r.open(hex(v["ct"]!), authenticating: hex(v["aad"]!))
        XCTAssertEqual(pt, hex(v["pt"]!))
    }

    /// The daemon's sealed payload (Rust, fixed ephemeral seed) opens here to
    /// the exact plaintext bytes the daemon produced.
    func testBatonVectorFromRust() throws {
        let v = try XCTUnwrap(try loadVectors()["baton_v1"] as? [String: Any])
        XCTAssertEqual(Data((v["info"] as! String).utf8), PushCrypto.info)
        let sk = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: hex(v["skR"] as! String))
        XCTAssertEqual(PushCrypto.publicKeyBase64(sk), v["public_key_b64"] as? String)

        let sealed = try XCTUnwrap(Data(base64Encoded: v["sealed_b64"] as! String))
        let pt = try PushCrypto.open(sealed, with: sk)
        XCTAssertEqual(pt, hex(v["plaintext_hex"] as! String))

        let n = try PushCrypto.decode(pt)
        let expected = v["notification"] as! [String: Any]
        XCTAssertEqual(n.kind, expected["kind"] as? String)
        XCTAssertEqual(n.title, expected["title"] as? String)
        XCTAssertEqual(n.body, expected["body"] as? String)
        XCTAssertEqual(n.session, expected["session"] as? String)
        XCTAssertEqual(n.ts, (expected["ts"] as? NSNumber)?.uint64Value)
    }

    /// The full NSE path: APNs userInfo as the relay builds it.
    func testUserInfoFromRelay() throws {
        let v = try XCTUnwrap(try loadVectors()["baton_v1"] as? [String: Any])
        let sk = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: hex(v["skR"] as! String))
        let userInfo: [AnyHashable: Any] = [
            "aps": ["alert": ["title": "Baton", "body": "New activity"], "mutable-content": 1],
            "baton": ["v": 1, "ct": v["sealed_b64"] as! String],
        ]
        let n = try PushCrypto.notification(fromUserInfo: userInfo, key: sk)
        XCTAssertEqual(n.title, "baton needs permission")

        XCTAssertThrowsError(try PushCrypto.notification(fromUserInfo: ["aps": [:]], key: sk)) {
            XCTAssertEqual($0 as? PushCrypto.Failure, .missingPayload)
        }
        XCTAssertThrowsError(
            try PushCrypto.notification(fromUserInfo: ["baton": ["v": 2, "ct": "AAAA"]], key: sk))
        // A different device key can't open it.
        XCTAssertThrowsError(
            try PushCrypto.notification(fromUserInfo: userInfo, key: PushCrypto.generatePrivateKey()))
    }

    func testRoundtripAndTamper() throws {
        let sk = PushCrypto.generatePrivateKey()
        let n = PushCrypto.Notification(kind: "finished", title: "t", body: "b", session: "s", ts: 1)
        let pt = try JSONEncoder().encode(n)
        let sealed = try PushCrypto.seal(pt, to: sk.publicKey)
        XCTAssertEqual(try PushCrypto.decode(try PushCrypto.open(sealed, with: sk)), n)

        var bad = sealed
        bad[bad.count - 1] ^= 1
        XCTAssertThrowsError(try PushCrypto.open(bad, with: sk))
        XCTAssertThrowsError(try PushCrypto.open(sealed.prefix(40), with: sk)) {
            XCTAssertEqual($0 as? PushCrypto.Failure, .malformed)
        }
    }

    func testRegistrationPush() {
        let info: [AnyHashable: Any] = [
            "aps": ["content-available": 1],
            "baton": ["v": 1, "type": "register", "env": "sandbox", "credential": String(repeating: "A", count: 43)],
        ]
        let r = PushCrypto.registration(fromUserInfo: info)
        XCTAssertEqual(r?.env, "sandbox")
        XCTAssertEqual(r?.credential.count, 43)
        XCTAssertNil(PushCrypto.registration(fromUserInfo: ["baton": ["v": 1, "ct": "x"]]))
    }
}
