// swift-tools-version:5.9
// PathDetect: "is this clipboard text a file path, and which file is it?"
// Pure Foundation (no AppKit/UIKit), so the iPhone app can reuse the
// detection for tappable paths in session transcripts. Used by the Mac app's
// copy-a-path-to-open watcher (apple/BatonMac, ClipWatcher).
import PackageDescription

let package = Package(
    name: "PathDetect",
    platforms: [.iOS(.v17), .macOS(.v14)],
    products: [.library(name: "PathDetect", targets: ["PathDetect"])],
    targets: [
        .target(name: "PathDetect"),
        .testTarget(name: "PathDetectTests", dependencies: ["PathDetect"]),
    ]
)
