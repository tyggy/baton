import XCTest
@testable import PathDetect

final class PathDetectorTests: XCTestCase {
    let home = "/Users/me"
    func d(_ s: String, exists: @escaping (String) -> Bool = { _ in false }) -> PathCandidate? {
        PathDetector.detect(s, home: home, exists: exists)
    }

    // MARK: accepted forms

    func testAbsolute() {
        let c = d("/Users/me/GitHub/baton/src/main.rs")
        XCTAssertEqual(c?.kind, .absolute)
        XCTAssertEqual(c?.path, "/Users/me/GitHub/baton/src/main.rs")
        XCTAssertNil(c?.line)
    }

    func testHome() {
        let c = d("~/GitHub/baton/README.md")
        XCTAssertEqual(c?.kind, .home)
        XCTAssertEqual(c?.path, "/Users/me/GitHub/baton/README.md")
    }

    func testFileURL() {
        let c = d("file:///Users/me/a%20b/x.swift")
        // decoded path with a space: allowed only because it's a file URL that exists
        XCTAssertNil(c)
        let c2 = d("file:///Users/me/a%20b/x.swift", exists: { $0 == "/Users/me/a b/x.swift" })
        XCTAssertEqual(c2?.path, "/Users/me/a b/x.swift")
        XCTAssertEqual(d("file:///tmp/x.rs#L40")?.line, 40)
        let c3 = d("file:///tmp/x.rs:12:3")
        XCTAssertEqual(c3?.path, "/tmp/x.rs")
        XCTAssertEqual(c3?.line, 12)
        XCTAssertEqual(c3?.column, 3)
    }

    func testRelative() {
        XCTAssertEqual(d("src/api/read.rs")?.kind, .relative)
        XCTAssertEqual(d("./scripts/run.sh")?.kind, .relative)
        XCTAssertEqual(d("../other/Cargo.toml")?.kind, .relative)
        XCTAssertEqual(d("docs/")?.kind, .relative)
        XCTAssertEqual(d("apple/BatonMac")?.kind, .relative)
    }

    func testBareNames() {
        XCTAssertEqual(d("read.rs")?.kind, .bareName)
        XCTAssertEqual(d("api.md")?.kind, .bareName)
        XCTAssertEqual(d("Cargo.toml")?.kind, .bareName)
        XCTAssertEqual(d(".env")?.kind, .bareName)
        XCTAssertEqual(d(".gitignore")?.kind, .bareName)
        XCTAssertEqual(d("main.c")?.kind, .bareName)
        XCTAssertEqual(d("foo.test.ts")?.kind, .bareName)
    }

    // MARK: suffixes

    func testLineSuffixes() {
        var c = d("src/api/read.rs:120")
        XCTAssertEqual(c?.path, "src/api/read.rs")
        XCTAssertEqual(c?.line, 120)
        XCTAssertNil(c?.column)

        c = d("read.rs:120:7")
        XCTAssertEqual(c?.path, "read.rs")
        XCTAssertEqual(c?.line, 120)
        XCTAssertEqual(c?.column, 7)

        c = d("docs/api.md#L12")
        XCTAssertEqual(c?.path, "docs/api.md")
        XCTAssertEqual(c?.line, 12)

        c = d("docs/api.md#L12-L20")
        XCTAssertEqual(c?.line, 12)

        c = d("read.rs:120-135")
        XCTAssertEqual(c?.path, "read.rs")
        XCTAssertEqual(c?.line, 120)

        XCTAssertNil(d("read.rs:0")?.line)
        XCTAssertEqual(c?.displayName, "read.rs:120")
    }

    // MARK: trimming

    func testTrimming() {
        XCTAssertEqual(d("  `src/api/read.rs`  ")?.path, "src/api/read.rs")
        XCTAssertEqual(d("\"docs/api.md\"")?.path, "docs/api.md")
        XCTAssertEqual(d("'read.rs'")?.path, "read.rs")
        XCTAssertEqual(d("@src/api/read.rs")?.path, "src/api/read.rs")
        XCTAssertEqual(d("`@docs/api.md`")?.path, "docs/api.md")
        XCTAssertEqual(d("read.rs.")?.path, "read.rs")
        XCTAssertEqual(d("read.rs,")?.path, "read.rs")
        XCTAssertEqual(d("(src/lib.rs)")?.path, "src/lib.rs")
        XCTAssertEqual(d("src/lib.rs:")?.path, "src/lib.rs")
        XCTAssertEqual(d("src/lib.rs:42.")?.line, 42)
        XCTAssertEqual(d("\n  read.rs \n")?.path, "read.rs")
        XCTAssertEqual(d("`../src/x.rs`.")?.path, "../src/x.rs")
    }

    // MARK: rejects

    func testRejectsURLs() {
        XCTAssertNil(d("https://github.com/x/y/blob/main/a.rs"))
        XCTAssertNil(d("http://localhost:3000/api"))
        XCTAssertNil(d("ssh://host/path/x.rs"))
        XCTAssertNil(d("mailto:a@b.com"))
        XCTAssertNil(d("www.example.com/x.html"))
        XCTAssertNil(d("git@github.com:org/repo.git"))
        XCTAssertNil(d("example.com"))
        XCTAssertNil(d("anthropic.ai"))
    }

    func testRejectsPlainWordsAndNumbers() {
        XCTAssertNil(d("hello"))
        XCTAssertNil(d("README"))
        XCTAssertNil(d("3.14"))
        XCTAssertNil(d("v1.2.3"))
        XCTAssertNil(d("1/2"))
        XCTAssertNil(d("2026/09/27"))
        XCTAssertNil(d("and/or"))
        XCTAssertNil(d("e.g"))
        XCTAssertNil(d("~"))
        XCTAssertNil(d("/"))
        XCTAssertNil(d("//comment"))
        XCTAssertNil(d(""))
        XCTAssertNil(d("   "))
        XCTAssertNil(d("..."))
    }

    func testRejectsSpacesUnlessExistingAbsolute() {
        XCTAssertNil(d("see src/api/read.rs"))
        XCTAssertNil(d("read.rs and api.md"))
        XCTAssertNil(d("my file.txt"))
        XCTAssertNil(d("/Users/me/My Docs/a.txt"))
        let c = d("/Users/me/My Docs/a.txt", exists: { $0 == "/Users/me/My Docs/a.txt" })
        XCTAssertEqual(c?.kind, .absolute)
        let h = d("~/My Docs/a.txt", exists: { $0 == "/Users/me/My Docs/a.txt" })
        XCTAssertEqual(h?.path, "/Users/me/My Docs/a.txt")
        XCTAssertNil(d("src/My Docs/a.txt", exists: { _ in true }))
    }

    func testRejectsMultilineAndLong() {
        XCTAssertNil(d("src/a.rs\nsrc/b.rs"))
        XCTAssertNil(d(String(repeating: "a/", count: 300) + "x.rs"))
    }

    func testRejectsShellish() {
        XCTAssertNil(d("rm -rf /"))
        XCTAssertNil(d("src/*.rs"))
        XCTAssertNil(d("$HOME/x.rs"))
        XCTAssertNil(d("a.rs|b"))
        XCTAssertNil(d("cat a.rs"))
        XCTAssertNil(d("foo;bar.rs"))
    }
}
