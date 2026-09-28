import CryptoKit
import Foundation

/// Baton push payload crypto (docs/relay.md).
///
/// The Mac daemon seals each notification to this device's X25519 key with
/// HPKE (RFC 9180, base mode, DHKEM(X25519, HKDF-SHA256) / HKDF-SHA256 /
/// ChaCha20-Poly1305 = `HPKE.Ciphersuite.Curve25519_SHA256_ChachaPoly`).
/// The relay forwards `base64(enc || ciphertext)` to APNs under
/// `baton.ct`; the Notification Service Extension opens it here and
/// replaces the generic "Baton" alert with the decrypted title and body.
public enum PushCrypto {
    /// HPKE `info`; must match `HPKE_INFO` in crates/baton-daemon/src/push.rs.
    public static let info = Data("baton push v1".utf8)
    /// HPKE `aad` (unused).
    public static let aad = Data()
    public static let ciphersuite = HPKE.Ciphersuite.Curve25519_SHA256_ChachaPoly
    /// Length of the encapsulated key that prefixes every sealed message.
    public static let encLength = 32
    /// APNs payload key the relay puts the ciphertext under.
    public static let payloadKey = "baton"

    public enum Failure: Error, Equatable {
        case missingPayload
        case unsupportedVersion(Int)
        case malformed
    }

    /// The decrypted notification (the daemon's `push::Notification`).
    public struct Notification: Codable, Equatable, Sendable {
        public var v: Int
        public var kind: String
        public var title: String
        public var body: String
        public var session: String?
        public var ts: UInt64

        public init(v: Int = 1, kind: String, title: String, body: String, session: String? = nil, ts: UInt64) {
            self.v = v
            self.kind = kind
            self.title = title
            self.body = body
            self.session = session
            self.ts = ts
        }
    }

    // MARK: keys

    /// A new device key. Store `rawRepresentation` in a Keychain access group
    /// shared with the Notification Service Extension.
    public static func generatePrivateKey() -> Curve25519.KeyAgreement.PrivateKey {
        Curve25519.KeyAgreement.PrivateKey()
    }

    /// The `public_key` field for `POST /api/push/register`.
    public static func publicKeyBase64(_ key: Curve25519.KeyAgreement.PrivateKey) -> String {
        key.publicKey.rawRepresentation.base64EncodedString()
    }

    // MARK: open / seal

    /// Open `enc || ciphertext`.
    public static func open(_ sealed: Data, with key: Curve25519.KeyAgreement.PrivateKey) throws -> Data {
        guard sealed.count >= encLength + 16 else { throw Failure.malformed }
        let enc = sealed.prefix(encLength)
        let ct = sealed.dropFirst(encLength)
        var recipient = try HPKE.Recipient(
            privateKey: key, ciphersuite: ciphersuite, info: info, encapsulatedKey: Data(enc))
        return try recipient.open(Data(ct), authenticating: aad)
    }

    /// Seal to a device public key (the daemon does this in Rust; here for
    /// tests and tooling). Returns `enc || ciphertext`.
    public static func seal(_ plaintext: Data, to publicKey: Curve25519.KeyAgreement.PublicKey) throws -> Data {
        var sender = try HPKE.Sender(recipientKey: publicKey, ciphersuite: ciphersuite, info: info)
        let ct = try sender.seal(plaintext, authenticating: aad)
        return sender.encapsulatedKey + ct
    }

    /// Decode a decrypted plaintext (compact JSON plus space padding).
    public static func decode(_ plaintext: Data) throws -> Notification {
        let n = try JSONDecoder().decode(Notification.self, from: plaintext)
        guard n.v == 1 else { throw Failure.unsupportedVersion(n.v) }
        return n
    }

    /// Everything the Notification Service Extension needs: pull
    /// `baton.ct` out of the APNs `userInfo`, open and decode it.
    public static func notification(
        fromUserInfo userInfo: [AnyHashable: Any], key: Curve25519.KeyAgreement.PrivateKey
    ) throws -> Notification {
        guard let box = userInfo[payloadKey] as? [String: Any],
              let ct = box["ct"] as? String
        else { throw Failure.missingPayload }
        if let v = box["v"] as? Int, v != 1 { throw Failure.unsupportedVersion(v) }
        guard let sealed = Data(base64Encoded: ct) else { throw Failure.malformed }
        return try decode(try open(sealed, with: key))
    }

    /// The registration push from the relay (`baton.type == "register"`):
    /// returns the send credential and APNs environment to pass to the
    /// daemon, or nil for any other push.
    public static func registration(fromUserInfo userInfo: [AnyHashable: Any]) -> (credential: String, env: String)? {
        guard let box = userInfo[payloadKey] as? [String: Any],
              box["type"] as? String == "register",
              let credential = box["credential"] as? String,
              let env = box["env"] as? String
        else { return nil }
        return (credential, env)
    }
}
