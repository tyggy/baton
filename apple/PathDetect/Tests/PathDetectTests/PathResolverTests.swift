import XCTest
@testable import PathDetect

/// Fixture tree (under a temp dir):
///
///   repoA/.git/
///   repoA/Cargo.toml
///   repoA/docs/api.md
///   repoA/crates/daemon/src/api/read.rs
///   repoA/crates/core/src/lib.rs
///   repoA/node_modules/pkg/read.rs      (skipped dir)
///   repoA/target/debug/read.rs          (skipped dir)
///   repoA/app/sub/                       (a cwd below the root)
///   repoB/.git/
///   repoB/docs/api.md
///   repoB/src/lib.rs
///   repoB/src/util/lib.rs
///   deep/a/b/c/d/e/f/g/deep.txt          (depth 8: out of range)
final class PathResolverTests: XCTestCase {
    var root: String!

    override func setUpWithError() throws {
        root = (NSTemporaryDirectory() as NSString).appendingPathComponent("pathdetect-\(UUID().uuidString)")
        let files = [
            "repoA/Cargo.toml", "repoA/docs/api.md", "repoA/crates/daemon/src/api/read.rs",
            "repoA/crates/core/src/lib.rs", "repoA/node_modules/pkg/read.rs", "repoA/target/debug/read.rs",
            "repoB/docs/api.md", "repoB/src/lib.rs", "repoB/src/util/lib.rs",
            "deep/a/b/c/d/e/f/g/deep.txt", "space dir/my file.txt",
        ]
        let fm = FileManager.default
        for f in files {
            let p = (root as NSString).appendingPathComponent(f)
            try fm.createDirectory(atPath: (p as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
            try Data("x".utf8).write(to: URL(fileURLWithPath: p))
        }
        for d in ["repoA/.git", "repoB/.git", "repoA/app/sub"] {
            try fm.createDirectory(atPath: (root as NSString).appendingPathComponent(d), withIntermediateDirectories: true)
        }
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(atPath: root)
    }

    func p(_ rel: String) -> String { ((root as NSString).appendingPathComponent(rel) as NSString).standardizingPath }
    func cand(_ s: String) -> PathCandidate { PathDetector.detect(s, home: "/nonexistent-home")! }
    var resolver: PathResolver { PathResolver(options: ResolverOptions(home: "/nonexistent-home")) }

    func testAbsoluteExistsOrNot() {
        let r = resolver.resolve(cand(p("repoA/Cargo.toml") + ":3"), bases: [])
        XCTAssertEqual(r.outcome, .one)
        XCTAssertEqual(r.matches.first?.path, p("repoA/Cargo.toml"))
        XCTAssertNil(r.matches.first?.base)
        XCTAssertEqual(r.candidate.line, 3)
        XCTAssertEqual(resolver.resolve(cand(p("repoA/nope.rs")), bases: []).outcome, .none)
    }

    func testAbsoluteWithSpaces() {
        let c = PathDetector.detect(p("space dir/my file.txt"))!
        XCTAssertEqual(resolver.resolve(c, bases: []).outcome, .one)
    }

    func testDirectoryMatch() {
        let r = resolver.resolve(cand("docs/"), bases: [PathBase(path: p("repoA"), source: "A", tier: 0)])
        XCTAssertEqual(r.outcome, .one)
        XCTAssertTrue(r.matches[0].isDirectory)
    }

    func testRelativeAgainstFrontmostBase() {
        let bases = [PathBase(path: p("repoA"), source: "front", tier: 0),
                     PathBase(path: p("repoB"), source: "recent", tier: 1)]
        let r = resolver.resolve(cand("docs/api.md:12"), bases: bases)
        XCTAssertEqual(r.outcome, .one, "the frontmost tier wins over a recent session with the same file")
        XCTAssertEqual(r.matches[0].path, p("repoA/docs/api.md"))
        XCTAssertEqual(r.matches[0].base?.source, "front")
    }

    func testRelativeFallsThroughToLaterTier() {
        let bases = [PathBase(path: p("repoA"), source: "front", tier: 0),
                     PathBase(path: p("repoB"), source: "recent", tier: 1)]
        let r = resolver.resolve(cand("src/util/lib.rs"), bases: bases)
        XCTAssertEqual(r.outcome, .one)
        XCTAssertEqual(r.matches[0].path, p("repoB/src/util/lib.rs"))
    }

    func testRelativeSameTierIsAmbiguous() {
        let bases = [PathBase(path: p("repoA"), source: "A", tier: 1),
                     PathBase(path: p("repoB"), source: "B", tier: 1)]
        let r = resolver.resolve(cand("docs/api.md"), bases: bases)
        XCTAssertEqual(r.outcome, .several)
        XCTAssertEqual(Set(r.matches.map(\.path)), [p("repoA/docs/api.md"), p("repoB/docs/api.md")])
    }

    func testRelativeSuffixSearch() {
        // Claude often prints a path relative to a crate, not the workspace root.
        let r = resolver.resolve(cand("src/api/read.rs:120"), bases: [PathBase(path: p("repoA"), source: "A", tier: 0)])
        XCTAssertEqual(r.outcome, .one)
        XCTAssertEqual(r.matches[0].path, p("repoA/crates/daemon/src/api/read.rs"))
    }

    func testBareNameSearchSkipsVendoredDirs() {
        let r = resolver.resolve(cand("read.rs"), bases: [PathBase(path: p("repoA"), source: "A", tier: 0)])
        XCTAssertEqual(r.outcome, .one, "node_modules and target are skipped: \(r.matches.map(\.path))")
        XCTAssertEqual(r.matches[0].path, p("repoA/crates/daemon/src/api/read.rs"))
    }

    func testBareNameDirectJoinFirst() {
        let r = resolver.resolve(cand("Cargo.toml"), bases: [PathBase(path: p("repoA"), source: "A", tier: 0)])
        XCTAssertEqual(r.matches.map(\.path), [p("repoA/Cargo.toml")])
    }

    func testBareNameAmbiguous() {
        let r = resolver.resolve(cand("lib.rs"), bases: [PathBase(path: p("repoB"), source: "B", tier: 0)])
        XCTAssertEqual(r.outcome, .several)
        XCTAssertEqual(r.matches.map(\.path), [p("repoB/src/lib.rs"), p("repoB/src/util/lib.rs")],
                       "breadth-first: shallower first")
    }

    func testDepthLimit() {
        let r = resolver.resolve(cand("deep.txt"), bases: [PathBase(path: p("deep"), source: "D", tier: 0)])
        XCTAssertEqual(r.outcome, .none)
        var o = ResolverOptions(home: "/nonexistent-home")
        o.maxDepth = 10
        XCTAssertEqual(PathResolver(options: o).resolve(cand("deep.txt"), bases: [PathBase(path: p("deep"), source: "D", tier: 0)]).outcome, .one)
    }

    func testNoMatch() {
        let r = resolver.resolve(cand("missing.rs"), bases: [PathBase(path: p("repoA"), source: "A", tier: 0)])
        XCTAssertEqual(r.outcome, .none)
        XCTAssertEqual(resolver.resolve(cand("missing.rs"), bases: []).outcome, .none)
    }

    func testDeadline() {
        var o = ResolverOptions(home: "/nonexistent-home")
        o.deadline = 0
        let r = PathResolver(options: o).resolve(cand("read.rs"), bases: [PathBase(path: p("repoA"), source: "A", tier: 0)])
        XCTAssertEqual(r.outcome, .none)
        XCTAssertTrue(r.searchTruncated)
    }

    func testSkipPaths() {
        var o = ResolverOptions(home: "/nonexistent-home")
        o.skipPaths.insert(p("repoA/crates"))
        let r = PathResolver(options: o).resolve(cand("read.rs"), bases: [PathBase(path: p("repoA"), source: "A", tier: 0)])
        XCTAssertEqual(r.outcome, .none)
    }

    func testGitRootAndBases() {
        XCTAssertEqual(PathResolver.gitRoot(of: p("repoA/app/sub")), p("repoA"))
        XCTAssertNil(PathResolver.gitRoot(of: p("deep/a")))
        let bases = PathResolver.bases(cwds: [(p("repoA/app/sub"), "tmux x", 0), (p("repoB"), "tmux y", 1)])
        XCTAssertEqual(bases.map(\.path), [p("repoA/app/sub"), p("repoA"), p("repoB")])
        XCTAssertEqual(bases.map(\.tier), [0, 1, 2])
        // cwd below the repo root: a root-relative path still resolves via the git root.
        let r = resolver.resolve(cand("docs/api.md"), bases: bases)
        XCTAssertEqual(r.matches.map(\.path), [p("repoA/docs/api.md")])
        XCTAssertEqual(r.matches.first?.base?.source, "tmux x · git root")
    }

    func testSymlinkedDuplicatesCollapse() throws {
        try FileManager.default.createSymbolicLink(atPath: p("linkA"), withDestinationPath: p("repoA"))
        let bases = [PathBase(path: p("repoA"), source: "A", tier: 0), PathBase(path: p("linkA"), source: "L", tier: 0)]
        XCTAssertEqual(resolver.resolve(cand("Cargo.toml"), bases: bases).outcome, .one)
    }
}
