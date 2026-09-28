import Foundation

/// A directory a partial path may be relative to, with where it came from
/// ("tmux claude-1a2b", "git root", …) for display.
public struct PathBase: Equatable, Hashable, Sendable {
    public var path: String
    public var source: String
    /// Lower tiers are tried first; the first tier with any hit wins.
    public var tier: Int

    public init(path: String, source: String, tier: Int) {
        self.path = (path as NSString).standardizingPath
        self.source = source
        self.tier = tier
    }
}

public struct PathMatch: Equatable, Hashable, Sendable {
    public var path: String
    public var isDirectory: Bool
    /// The base it resolved against (nil for absolute paths).
    public var base: PathBase?

    public init(path: String, isDirectory: Bool, base: PathBase?) {
        self.path = path
        self.isDirectory = isDirectory
        self.base = base
    }
}

public struct Resolution: Equatable, Sendable {
    public enum Outcome: Equatable, Sendable { case none, one, several }
    public var candidate: PathCandidate
    public var matches: [PathMatch]
    /// True when the bounded search stopped at its deadline or file budget.
    public var searchTruncated: Bool

    public var outcome: Outcome {
        switch matches.count {
        case 0: .none
        case 1: .one
        default: .several
        }
    }
}

public struct ResolverOptions: Sendable {
    public var maxDepth = 6
    public var deadline: TimeInterval = 0.2
    public var maxMatches = 12
    /// Directory names never descended into.
    public var skipNames: Set<String> = [
        ".git", "node_modules", "target", "build", ".build", ".venv", "venv",
        "DerivedData", "Pods", ".next", "__pycache__", ".cache", ".Trash",
    ]
    /// Absolute directories never descended into during a search (TCC-guarded
    /// or huge). A base inside one of them is still searched.
    public var skipPaths: Set<String>

    public init(home: String = NSHomeDirectory()) {
        let h = (home as NSString).standardizingPath
        skipPaths = Set(["Library", "Documents", "Desktop", "Downloads", "Pictures",
                         "Movies", "Music", "Applications", "Public", ".Trash"]
            .map { (h as NSString).appendingPathComponent($0) })
        skipPaths.formUnion(["/Volumes", "/System", "/private", "/dev", "/Library"])
    }
}

/// Turns a `PathCandidate` into existing files. Pure Foundation; no AppKit.
public struct PathResolver: Sendable {
    public var options: ResolverOptions

    public init(options: ResolverOptions = ResolverOptions()) {
        self.options = options
    }

    /// Resolve against `bases` (any order; tiers decide priority).
    ///
    /// - absolute / `~` / `file://`: exists or not; bases unused.
    /// - relative (`src/api/read.rs`): joined to each base; if nothing joins,
    ///   a bounded search for files ending in `/src/api/read.rs`.
    /// - bare name (`read.rs`): joined to each base, then a bounded search.
    ///
    /// Tier by tier: the first tier with any hit is the answer, so the
    /// frontmost session's repo beats every other recent session.
    public func resolve(_ c: PathCandidate, bases: [PathBase]) -> Resolution {
        let fm = FileManager.default
        if c.isAbsolute {
            var isDir: ObjCBool = false
            let p = (c.path as NSString).standardizingPath
            guard fm.fileExists(atPath: p, isDirectory: &isDir) else {
                return Resolution(candidate: c, matches: [], searchTruncated: false)
            }
            return Resolution(candidate: c, matches: [PathMatch(path: p, isDirectory: isDir.boolValue, base: nil)],
                              searchTruncated: false)
        }

        let tiers = Dictionary(grouping: dedupe(bases), by: \.tier).sorted { $0.key < $1.key }
        let deadline = Date().addingTimeInterval(options.deadline)
        var truncated = false

        for (_, tierBases) in tiers {
            // 1. Direct joins.
            var hits: [PathMatch] = []
            for b in tierBases {
                let p = ((b.path as NSString).appendingPathComponent(c.path) as NSString).standardizingPath
                var isDir: ObjCBool = false
                if fm.fileExists(atPath: p, isDirectory: &isDir) {
                    hits.append(PathMatch(path: p, isDirectory: isDir.boolValue, base: b))
                }
            }
            hits = uniqueByPath(hits)
            if !hits.isEmpty { return Resolution(candidate: c, matches: hits, searchTruncated: false) }

            // 2. Bounded search below each base of this tier.
            let suffix = c.kind == .relative ? normalizedRelative(c.path) : nil
            guard c.kind == .bareName || (suffix.map { !$0.hasPrefix("../") } ?? false) else { continue }
            let name = (c.path as NSString).lastPathComponent
            var found: [PathMatch] = []
            var visited = Set<String>()
            for b in tierBases {
                let r = search(base: b, name: name, suffix: suffix, deadline: deadline, visited: &visited)
                found += r.matches
                truncated = truncated || r.truncated
                if found.count >= options.maxMatches { break }
            }
            found = uniqueByPath(found)
            if !found.isEmpty {
                return Resolution(candidate: c, matches: Array(found.prefix(options.maxMatches)),
                                  searchTruncated: truncated)
            }
        }
        return Resolution(candidate: c, matches: [], searchTruncated: truncated)
    }

    // MARK: search

    struct SearchResult {
        var matches: [PathMatch]
        var truncated: Bool
    }

    /// Breadth-first so shallow hits come first and the deadline cuts off the
    /// deep, unlikely part of the tree.
    func search(base: PathBase, name: String, suffix: String?, deadline: Date,
                visited: inout Set<String>) -> SearchResult {
        let fm = FileManager.default
        var queue: [(String, Int)] = [(base.path, 0)]
        var head = 0
        var out: [PathMatch] = []
        let wantSuffix = suffix.map { "/" + $0 }
        while head < queue.count {
            if Date() > deadline { return SearchResult(matches: out, truncated: true) }
            let (dir, depth) = queue[head]
            head += 1
            guard visited.insert(dir).inserted else { continue }
            let keys: [URLResourceKey] = [.isDirectoryKey, .isSymbolicLinkKey, .isPackageKey]
            guard let entries = try? fm.contentsOfDirectory(
                at: URL(fileURLWithPath: dir, isDirectory: true), includingPropertiesForKeys: keys, options: []
            ) else { continue }
            for u in entries.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }) {
                let e = u.lastPathComponent
                let full = (dir as NSString).appendingPathComponent(e)
                let rv = try? u.resourceValues(forKeys: Set(keys))
                let isLink = rv?.isSymbolicLink ?? false
                var isDir = rv?.isDirectory ?? false
                if isLink {
                    var d: ObjCBool = false
                    isDir = fm.fileExists(atPath: full, isDirectory: &d) && d.boolValue
                }
                if e == name {
                    if let w = wantSuffix {
                        if full.hasSuffix(w) { out.append(PathMatch(path: full, isDirectory: isDir, base: base)) }
                    } else {
                        out.append(PathMatch(path: full, isDirectory: isDir, base: base))
                    }
                    if out.count >= options.maxMatches { return SearchResult(matches: out, truncated: true) }
                }
                // Don't follow symlinked directories (loops) or enter bundles.
                if isDir, !isLink, !(rv?.isPackage ?? false), depth + 1 <= options.maxDepth,
                   !options.skipNames.contains(e), !options.skipPaths.contains(full) {
                    queue.append((full, depth + 1))
                }
            }
        }
        return SearchResult(matches: out, truncated: false)
    }

    // MARK: helpers

    func normalizedRelative(_ p: String) -> String {
        var s = p
        while s.hasPrefix("./") { s.removeFirst(2) }
        while s.hasSuffix("/") && s.count > 1 { s.removeLast() }
        return s
    }

    func dedupe(_ bases: [PathBase]) -> [PathBase] {
        var seen = Set<String>()
        var out: [PathBase] = []
        for b in bases.sorted(by: { $0.tier < $1.tier }) where !b.path.isEmpty {
            if seen.insert(b.path).inserted { out.append(b) }
        }
        return out
    }

    func uniqueByPath(_ ms: [PathMatch]) -> [PathMatch] {
        var seen = Set<String>()
        return ms.filter { seen.insert(($0.path as NSString).resolvingSymlinksInPath).inserted }
    }

    /// Nearest ancestor (or self) containing `.git`, or nil.
    public static func gitRoot(of dir: String) -> String? {
        var d = (dir as NSString).standardizingPath
        let fm = FileManager.default
        while d != "/" && !d.isEmpty {
            if fm.fileExists(atPath: (d as NSString).appendingPathComponent(".git")) { return d }
            d = (d as NSString).deletingLastPathComponent
        }
        return nil
    }

    /// Bases from ordered cwds: each cwd, then its git root just after it
    /// (tier `2t` for the cwd, `2t+1` for its root): the session's own
    /// directory wins, the repo root is next, then the next session.
    public static func bases(cwds: [(path: String, source: String, tier: Int)]) -> [PathBase] {
        var out: [PathBase] = []
        for c in cwds {
            out.append(PathBase(path: c.path, source: c.source, tier: c.tier * 2))
            if let g = gitRoot(of: c.path), g != (c.path as NSString).standardizingPath {
                out.append(PathBase(path: g, source: "\(c.source) · git root", tier: c.tier * 2 + 1))
            }
        }
        return out
    }
}
