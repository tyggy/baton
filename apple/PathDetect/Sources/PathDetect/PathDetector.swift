import Foundation

/// A piece of text that looks like a file path, split into the path and an
/// optional `:line[:col]` / `#L12` location.
public struct PathCandidate: Equatable, Sendable {
    public enum Kind: String, Equatable, Sendable {
        /// `/Users/…`
        case absolute
        /// `~/…` (already expanded in `path`)
        case home
        /// `file:///…` (already decoded in `path`)
        case fileURL
        /// `src/api/read.rs`, `./x`, `../x`
        case relative
        /// `read.rs`, `.env`
        case bareName
    }

    /// The trimmed text the candidate came from (location suffix included).
    public var text: String
    /// Path without the location suffix; absolute for `.absolute`, `.home`, `.fileURL`.
    public var path: String
    public var kind: Kind
    public var line: Int?
    public var column: Int?

    public init(text: String, path: String, kind: Kind, line: Int? = nil, column: Int? = nil) {
        self.text = text
        self.path = path
        self.kind = kind
        self.line = line
        self.column = column
    }

    public var isAbsolute: Bool { kind == .absolute || kind == .home || kind == .fileURL }

    /// `read.rs:120` style label for UI.
    public var displayName: String {
        let name = (path as NSString).lastPathComponent
        guard let line else { return name }
        return column.map { "\(name):\(line):\($0)" } ?? "\(name):\(line)"
    }
}

/// Strict "the whole clipboard is a path" test. False negatives are cheap
/// (nothing happens); false positives open things the user didn't mean, so
/// anything doubtful is rejected.
public enum PathDetector {
    public static let maxLength = 512

    /// Extensions that make a bare word look like a domain, not a file.
    static let domainLikeExtensions: Set<String> = ["com", "org", "net", "io", "ai", "co", "gov", "edu", "uk", "de", "ru", "us", "info", "biz"]
    /// One-letter extensions that are really file types.
    static let oneLetterExtensions: Set<String> = ["c", "h", "m", "s", "r", "d", "v", "o", "a"]

    /// - Parameters:
    ///   - text: the raw clipboard string.
    ///   - home: home directory used to expand `~/`.
    ///   - exists: file-existence check, only consulted for absolute paths
    ///     that contain spaces (the one case a space is allowed).
    public static func detect(
        _ text: String,
        home: String = NSHomeDirectory(),
        exists: (String) -> Bool = { FileManager.default.fileExists(atPath: $0) }
    ) -> PathCandidate? {
        guard text.utf8.count <= maxLength * 4 else { return nil }
        var s = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !s.isEmpty, s.count <= maxLength else { return nil }
        guard !s.contains(where: { $0.isNewline }) else { return nil }
        s = trimDecorations(s)
        guard !s.isEmpty else { return nil }

        // file:// URLs.
        if s.lowercased().hasPrefix("file://") {
            guard let url = URL(string: s) ?? URL(string: s.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? ""),
                  url.isFileURL else { return nil }
            var line: Int?, col: Int?
            if let frag = url.fragment, let l = parseHashLine(frag) { line = l }
            let p = url.path
            guard p.hasPrefix("/"), p.count > 1 else { return nil }
            if p.contains(where: { $0.isWhitespace }) && !exists(p) { return nil }
            var cand = PathCandidate(text: s, path: p, kind: .fileURL, line: line, column: col)
            // `file:///x.rs:12` — the colon suffix sits in the path.
            if line == nil, let (base, l, c) = splitColonSuffix(p) {
                cand.path = base; line = l; col = c
                cand.line = l; cand.column = c
            }
            return cand
        }

        // Any other scheme (http:, mailto:, ssh://, x-apple…) is out of scope.
        if looksLikeURL(s) { return nil }

        // Location suffix.
        var body = s
        var line: Int?, col: Int?
        if let r = body.range(of: #"#L(\d+)(?:C(\d+))?(?:-L?\d+)?$"#, options: .regularExpression) {
            let frag = String(body[r].dropFirst())
            line = parseHashLine(frag)
            body = String(body[..<r.lowerBound])
        } else if let (b, l, c) = splitColonSuffix(body) {
            body = b; line = l; col = c
        }
        guard !body.isEmpty else { return nil }

        // Home / absolute.
        var kind: PathCandidate.Kind
        var path = body
        if body == "~" { return nil }
        if body.hasPrefix("~/") {
            kind = .home
            path = (home as NSString).appendingPathComponent(String(body.dropFirst(2)))
        } else if body.hasPrefix("/") {
            guard body.count > 1, !body.hasPrefix("//") else { return nil }
            kind = .absolute
        } else if body.contains("/") {
            kind = .relative
        } else {
            kind = .bareName
        }

        let hasSpace = body.contains(where: { $0.isWhitespace })
        if hasSpace {
            // Only an existing absolute path may contain spaces.
            guard kind == .absolute || kind == .home, exists(path) else { return nil }
        }
        guard charactersOK(body) else { return nil }

        switch kind {
        case .relative:
            guard relativeOK(body) else { return nil }
        case .bareName:
            guard bareNameOK(body) else { return nil }
        case .absolute, .home:
            guard body.contains(where: { $0.isLetter }) else { return nil }
        case .fileURL:
            break
        }
        return PathCandidate(text: s, path: path, kind: kind, line: line, column: col)
    }

    // MARK: pieces (internal for tests)

    /// Strip whitespace, backticks, quotes, a leading `@` (Claude's `@path`
    /// mentions), wrapping parens/brackets and trailing sentence punctuation.
    static func trimDecorations(_ input: String) -> String {
        var s = input
        let wrap: Set<Character> = ["`", "\"", "'", "“", "”", "‘", "’", "<", ">", "[", "]"]
        let trailing: Set<Character> = [".", ",", ";", ":", ")", "!", "?"]
        var changed = true
        while changed && !s.isEmpty {
            changed = false
            let before = s
            s = s.trimmingCharacters(in: .whitespaces)
            while let f = s.first, wrap.contains(f) || f == "(" { s.removeFirst() }
            while let l = s.last, wrap.contains(l) { s.removeLast() }
            if s.first == "@" { s.removeFirst() }
            while let l = s.last, trailing.contains(l) {
                // Keep `..` and `/.` endings (`../`, `foo/.`) intact.
                if l == ".", s.hasSuffix("..") || s.hasSuffix("/.") || s == "." { break }
                s.removeLast()
            }
            changed = s != before
        }
        return s
    }

    /// `path:12`, `path:12:5`, `path:12-20` → (path, 12, 5?).
    static func splitColonSuffix(_ s: String) -> (String, Int, Int?)? {
        guard let r = s.range(of: #":(\d{1,7})(?::(\d{1,5}))?(?:-\d{1,7})?$"#, options: .regularExpression) else { return nil }
        let base = String(s[..<r.lowerBound])
        guard !base.isEmpty, !base.hasSuffix(":") else { return nil }
        let nums = s[r].dropFirst().split(whereSeparator: { $0 == ":" || $0 == "-" })
        guard let line = nums.first.flatMap({ Int($0) }), line > 0 else { return nil }
        var col: Int?
        if s[r].filter({ $0 == ":" }).count == 2, nums.count >= 2 { col = Int(nums[1]) }
        return (base, line, col)
    }

    /// `L12`, `L12C3`, `L12-L20` → 12.
    static func parseHashLine(_ frag: String) -> Int? {
        guard frag.hasPrefix("L") else { return nil }
        let digits = frag.dropFirst().prefix(while: \.isNumber)
        guard let n = Int(digits), n > 0 else { return nil }
        return n
    }

    static func looksLikeURL(_ s: String) -> Bool {
        if s.range(of: #"^[A-Za-z][A-Za-z0-9+.\-]*://"#, options: .regularExpression) != nil { return true }
        if s.range(of: #"^(mailto|tel|sms|javascript|data|about|x-[a-z\-]+):"#, options: [.regularExpression, .caseInsensitive]) != nil { return true }
        if s.lowercased().hasPrefix("www.") { return true }
        // user@host:path (scp/git remote), e.g. git@github.com:org/repo.git
        if s.range(of: #"^[^/\s]+@[^/\s]+:"#, options: .regularExpression) != nil { return true }
        return false
    }

    /// No shell/glob metacharacters or control characters.
    static func charactersOK(_ s: String) -> Bool {
        let bad: Set<Character> = ["<", ">", "|", "*", "?", "{", "}", "$", "`", "\"", ";", "&", "\\", "\t"]
        return !s.contains(where: { bad.contains($0) || $0.isNewline || ($0.asciiValue.map { $0 < 0x20 } ?? false) })
    }

    static func relativeOK(_ s: String) -> Bool {
        if s.contains("//") { return false }
        // Needs a letter somewhere: rejects 1/2, 2026/09/27.
        guard s.contains(where: { $0.isLetter }) else { return false }
        let parts = s.split(separator: "/", omittingEmptySubsequences: true)
        guard !parts.isEmpty else { return false }
        // `and/or`, `yes/no`: two short plain words with no dot anywhere.
        if parts.count == 2, !s.contains("."), !s.hasSuffix("/"),
           parts.allSatisfy({ $0.count <= 4 && $0.allSatisfy(\.isLetter) }) {
            return false
        }
        return true
    }

    static func bareNameOK(_ s: String) -> Bool {
        // Dotfiles: .env, .gitignore, .zshrc
        if s.hasPrefix(".") {
            return s.range(of: #"^\.[A-Za-z0-9_][A-Za-z0-9_.\-]*$"#, options: .regularExpression) != nil
        }
        guard s.range(of: #"^[A-Za-z0-9_@+\-][A-Za-z0-9_@+\-.]*\.([A-Za-z0-9]{1,12})$"#, options: .regularExpression) != nil else { return false }
        let ext = String(s[s.index(after: s.lastIndex(of: ".")!)...])
        let stem = String(s[..<s.lastIndex(of: ".")!])
        guard ext.contains(where: \.isLetter) else { return false }          // 3.14, v1.2
        guard stem.contains(where: { $0.isLetter || $0 == "_" }) else { return false }
        if ext.count == 1 && !oneLetterExtensions.contains(ext.lowercased()) { return false } // e.g
        if domainLikeExtensions.contains(ext.lowercased()) { return false }  // example.com
        return true
    }
}
