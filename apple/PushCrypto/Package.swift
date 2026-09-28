// swift-tools-version:5.9
// PushCrypto: opens Baton push payloads (HPKE, RFC 9180) with CryptoKit.
// Used by the iOS app (key generation, registration) and its Notification
// Service Extension (decryption). See docs/relay.md.
import PackageDescription

let package = Package(
    name: "PushCrypto",
    platforms: [.iOS(.v17), .macOS(.v14)],
    products: [.library(name: "PushCrypto", targets: ["PushCrypto"])],
    targets: [
        .target(name: "PushCrypto"),
        .testTarget(name: "PushCryptoTests", dependencies: ["PushCrypto"]),
    ]
)
