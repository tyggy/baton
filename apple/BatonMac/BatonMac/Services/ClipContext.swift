import AppKit
import Foundation
import PathDetect

/// Where a partial path copied from a terminal is probably relative to:
/// (a) the tmux session in the frontmost terminal tab, (b) sessions active in
/// the last 24 h (Claude sessions from the daemon's list first), (c) their git
/// roots. tmux is only *read* here (`list-clients`, `list-panes`).
enum ClipContext {
    struct Snapshot {
        var bases: [PathBase]
        var frontSession: String?
        var note: String
    }

    /// A row of `/api/local-sessions` we care about.
    struct DaemonRow: Decodable {
        var tmux_session: String?
        var last_ts: String?
    }

    static let tmuxCandidates = ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux", "/usr/bin/tmux"]
    static var tmuxPath: String? { tmuxCandidates.first { FileManager.default.isExecutableFile(atPath: $0) } }

    /// The tty of Terminal.app's selected tab — only if Baton already may
    /// automate Terminal (never prompts from here).
    static func terminalFrontTTY(frontBundleID: String?) -> String? {
        guard frontBundleID == Permissions.terminalBundleID,
              Permissions.automationStatus(ask: false).level == .ok else { return nil }
        let src = #"tell application id "com.apple.Terminal" to get tty of selected tab of front window"#
        var err: NSDictionary?
        let r = NSAppleScript(source: src)?.executeAndReturnError(&err)
        guard err == nil, let tty = r?.stringValue, tty.hasPrefix("/dev/") else { return nil }
        return tty
    }

    /// Off the main thread: tmux queries have a 1.5 s timeout each.
    static func gather(frontTTY: String?, frontIsTerminal: Bool, daemonRows: [DaemonRow],
                       extraBases: [String]) -> Snapshot {
        var cwds: [(path: String, source: String, tier: Int)] = []
        for (i, b) in extraBases.enumerated() { cwds.append((b, "--clip-base \(i + 1)", 0)) }
        var note: [String] = []
        var frontSession: String?

        guard let tmux = tmuxPath else {
            return Snapshot(bases: PathResolver.bases(cwds: cwds), frontSession: nil, note: "tmux not found")
        }
        // client tty → session
        let clients = Shell.run(tmux, ["list-clients", "-F", "#{client_tty}\t#{client_session}\t#{client_activity}"], timeout: 1.5)
        var byTTY: [String: String] = [:]
        var mostRecent: (String, Int)?
        for l in clients.stdout.split(separator: "\n") {
            let f = l.split(separator: "\t", omittingEmptySubsequences: false).map(String.init)
            guard f.count >= 3 else { continue }
            byTTY[f[0]] = f[1]
            let act = Int(f[2]) ?? 0
            if mostRecent == nil || act > mostRecent!.1 { mostRecent = (f[1], act) }
        }
        if let t = frontTTY, let s = byTTY[t] {
            frontSession = s
            note.append("front tab \(t) → \(s)")
        } else if frontTTY == nil, frontIsTerminal, let m = mostRecent {
            // No tty (another terminal app, or no Automation): the client
            // that saw input last is the one the user is looking at.
            frontSession = m.0
            note.append("most recent client → \(m.0)")
        }

        // Active pane cwd + activity per session.
        let panes = Shell.run(tmux, ["list-panes", "-a", "-F",
                                     "#{session_name}\t#{window_active}\t#{pane_active}\t#{session_activity}\t#{pane_current_path}"],
                              timeout: 1.5)
        var cwdOf: [String: String] = [:]
        var activity: [String: Int] = [:]
        for l in panes.stdout.split(separator: "\n") {
            let f = l.split(separator: "\t", maxSplits: 4, omittingEmptySubsequences: false).map(String.init)
            guard f.count == 5, !f[4].isEmpty else { continue }
            if f[1] == "1" && f[2] == "1" || cwdOf[f[0]] == nil { cwdOf[f[0]] = f[4] }
            activity[f[0]] = Int(f[3]) ?? 0
        }

        if let s = frontSession, let c = cwdOf[s] { cwds.append((c, "tmux \(s)", 0)) }

        // Claude sessions active in the last 24 h, newest first.
        let dayAgo = Date().addingTimeInterval(-86_400)
        let iso = ISO8601DateFormatter()
        iso.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let isoPlain = ISO8601DateFormatter()
        var claude: [(String, Date)] = []
        for r in daemonRows {
            guard let t = r.tmux_session, let ts = r.last_ts,
                  let d = iso.date(from: ts) ?? isoPlain.date(from: ts), d > dayAgo else { continue }
            claude.append((t, d))
        }
        var seen = Set(frontSession.map { [$0] } ?? [])
        for (s, _) in claude.sorted(by: { $0.1 > $1.1 }) where seen.insert(s).inserted {
            if let c = cwdOf[s] { cwds.append((c, "session \(s)", 1)) }
        }
        // Any other tmux session active in the last 24 h.
        let cutoff = Int(dayAgo.timeIntervalSince1970)
        for (s, a) in activity.sorted(by: { $0.value > $1.value }) where a >= cutoff && seen.insert(s).inserted {
            if let c = cwdOf[s] { cwds.append((c, "tmux \(s)", 2)) }
        }
        if !daemonRows.isEmpty { note.append("\(claude.count) recent Claude sessions") }
        var seenPath = Set<String>()
        cwds = cwds.filter { seenPath.insert(($0.path as NSString).standardizingPath).inserted }
        return Snapshot(bases: PathResolver.bases(cwds: Array(cwds.prefix(16))), frontSession: frontSession,
                        note: note.joined(separator: "; "))
    }

    // MARK: daemon list (cached)

    @MainActor private static var cache: (Date, [DaemonRow])?

    /// `GET /api/local-sessions` on the app's own daemon (the configured
    /// listen address), 1.5 s timeout, cached for 30 s. Empty on any failure.
    @MainActor static func daemonRows(listen: String) async -> [DaemonRow] {
        if let c = cache, Date().timeIntervalSince(c.0) < 30 { return c.1 }
        guard let url = URL(string: "http://\(listen)/api/local-sessions"),
              let token = Pairing.loadToken(), !LaunchOptions.isSnapshot else { return [] }
        var req = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 1.5)
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        struct Body: Decodable { var recent: [DaemonRow]?; var momentum: [DaemonRow]? }
        var rows: [DaemonRow] = []
        if let (data, resp) = try? await URLSession.shared.data(for: req),
           (resp as? HTTPURLResponse)?.statusCode == 200,
           let b = try? JSONDecoder().decode(Body.self, from: data) {
            rows = (b.recent ?? []) + (b.momentum ?? [])
        }
        cache = (Date(), rows)
        return rows
    }
}
