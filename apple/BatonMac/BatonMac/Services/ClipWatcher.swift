import AppKit
import OSLog
import PathDetect

/// "Copy a path → it opens." Polls the general pasteboard's change count
/// (0.4 s); when something new was copied in a terminal and the whole text is
/// path-like, resolves it against the sessions' directories and opens it.
///
/// Privacy: the change count, the type list and pattern detection don't
/// read contents. The string is read only after every cheap gate passes
/// (enabled, not paused, frontmost app is a terminal, not concealed or
/// transient, access not denied). On macOS 15.4+ that read may show the
/// system's "Baton would like to paste" alert until the user picks
/// Always Allow for Baton in System Settings → Privacy & Security → Paste.
@MainActor
final class ClipWatcher: ObservableObject {
    enum Mode: String, CaseIterable, Identifiable {
        case open, quickLook, ask
        var id: String { rawValue }
        var label: String {
            switch self {
            case .open: "Open"
            case .quickLook: "Quick Look"
            case .ask: "Ask First"
            }
        }
    }

    enum Access: Equatable {
        case notAsked, ask, allowed, denied, unknown
        var label: String {
            switch self {
            case .notAsked: "Not asked yet"
            case .ask: "Asks each time"
            case .allowed: "Always allowed"
            case .denied: "Denied"
            case .unknown: "Unknown"
            }
        }
    }

    static let defaultTerminals: [String] = [
        "com.apple.Terminal", "com.googlecode.iterm2", "com.mitchellh.ghostty", "dev.warp.Warp-Stable",
        "com.github.wez.wezterm", "net.kovidgoyal.kitty", "org.alacritty",
    ]

    // MARK: settings (UserDefaults)

    private let d = UserDefaults.standard
    @Published var enabled: Bool { didSet { d.set(enabled, forKey: "clip.enabled"); restart() } }
    @Published var paused: Bool { didSet { d.set(paused, forKey: "clip.paused") } }
    @Published var mode: Mode { didSet { d.set(mode.rawValue, forKey: "clip.mode") } }
    @Published var terminalOnly: Bool { didSet { d.set(terminalOnly, forKey: "clip.terminalOnly") } }
    @Published var terminalApps: [String] { didSet { d.set(terminalApps, forKey: "clip.terminalApps") } }
    @Published var showNoMatch: Bool { didSet { d.set(showNoMatch, forKey: "clip.showNoMatch") } }
    /// nil = the file's default app (line numbers when it's a known editor).
    @Published var lineEditor: String? { didSet { d.set(lineEditor, forKey: "clip.lineEditor") } }

    // MARK: state

    @Published private(set) var access: Access = .unknown
    @Published private(set) var lastEvent: String = "Nothing copied yet"

    let hud = ClipHUD()
    private var timer: Timer?
    private var lastChange = NSPasteboard.general.changeCount
    private var lastText: (String, Date)?
    private let log = Logger(subsystem: "com.torwood.baton.mac", category: "clip")
    /// `--clip-debug`: every step to stderr as well.
    private let debug = LaunchOptions.args.contains("--clip-debug")
    var extraBases: [String] = LaunchOptions.values("--clip-base")
    /// Set by AppModel so the list query uses the app's daemon address.
    var listenAddress: () -> String = { DaemonController.defaultListen }

    init() {
        d.register(defaults: [
            "clip.enabled": true, "clip.paused": false, "clip.mode": Mode.open.rawValue,
            "clip.terminalOnly": true, "clip.terminalApps": Self.defaultTerminals, "clip.showNoMatch": true,
        ])
        enabled = d.bool(forKey: "clip.enabled")
        paused = d.bool(forKey: "clip.paused")
        mode = Mode(rawValue: d.string(forKey: "clip.mode") ?? "") ?? .open
        terminalOnly = d.bool(forKey: "clip.terminalOnly")
        terminalApps = d.stringArray(forKey: "clip.terminalApps") ?? Self.defaultTerminals
        showNoMatch = d.bool(forKey: "clip.showNoMatch")
        lineEditor = d.string(forKey: "clip.lineEditor")
    }

    var active: Bool { enabled && !paused }

    func start() {
        refreshAccess()
        restart()
    }

    private func restart() {
        timer?.invalidate()
        timer = nil
        guard enabled, !LaunchOptions.isSnapshot, LaunchOptions.value("--clip-resolve") == nil else { return }
        lastChange = NSPasteboard.general.changeCount
        timer = Timer.scheduledTimer(withTimeInterval: 0.4, repeats: true) { _ in
            MainActor.assumeIsolated { AppModel.shared.clip.tick() }
        }
        timer?.tolerance = 0.1
    }

    func refreshAccess() {
        if #available(macOS 15.4, *) {
            access = switch NSPasteboard.general.accessBehavior {
            case .default: .notAsked
            case .ask: .ask
            case .alwaysAllow: .allowed
            case .alwaysDeny: .denied
            @unknown default: .unknown
            }
        } else {
            access = .allowed
        }
    }

    private func trace(_ s: String) {
        log.debug("\(s, privacy: .private)")
        if debug { FileHandle.standardError.write(Data("clip: \(s)\n".utf8)) }
    }

    // MARK: pipeline

    static let skipTypes: Set<String> = [
        "org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType", "org.nspasteboard.AutoGeneratedType",
        "com.agilebits.onepassword", "de.petermaurer.TransientPasteboardType",
    ]

    func tick() {
        let pb = NSPasteboard.general
        let cc = pb.changeCount
        guard cc != lastChange else { return }
        lastChange = cc
        guard active else { return }
        let front = NSWorkspace.shared.frontmostApplication
        let frontID = front?.bundleIdentifier ?? ""
        let isTerminal = terminalApps.contains(frontID)
        if terminalOnly && !isTerminal {
            trace("skip: front app \(frontID) is not a terminal")
            return
        }
        let types = Set((pb.types ?? []).map(\.rawValue))
        if !types.isDisjoint(with: Self.skipTypes) {
            trace("skip: concealed/transient")
            return
        }
        guard types.contains(NSPasteboard.PasteboardType.string.rawValue) else {
            trace("skip: no text")
            return
        }
        refreshAccess()
        trace("copy in \(frontID); paste access: \(access.label)")
        if access == .denied {
            trace("skip: pasteboard access denied")
            lastEvent = "Paste access is denied in System Settings"
            return
        }
        Task {
            if #available(macOS 15.4, *) {
                // Detection doesn't read the contents (no alert). A phone number
                // or an email is never a path; `read.rs` does count as a
                // "web URL" to the detector, so that pattern can't be a gate.
                if let p = try? await pb.detectedPatterns(for: [\.emailAddresses, \.phoneNumbers]),
                   !p.isEmpty {
                    trace("skip: detector says email/phone")
                    return
                }
            }
            guard pb.changeCount == cc else { return }
            // The read is where macOS may show its paste alert and wait for
            // an answer, so it runs off the main thread.
            let t0 = Date()
            let text = await Task.detached { NSPasteboard.general.string(forType: .string) }.value
            trace("read took \(Int(Date().timeIntervalSince(t0) * 1000)) ms")
            guard let text else {
                trace("skip: read returned nothing (denied?)")
                refreshAccess()
                return
            }
            refreshAccess()
            await handle(text: text, frontBundleID: frontID, frontIsTerminal: isTerminal)
        }
    }

    /// Detect → resolve → act. Also the entry point for `--clip-resolve`
    /// and the Settings test field (`act: false` just reports).
    @discardableResult
    func handle(text: String, frontBundleID: String?, frontIsTerminal: Bool, act: Bool = true) async -> (Resolution?, ClipContext.Snapshot?) {
        guard let cand = PathDetector.detect(text) else {
            trace("not path-like")
            return (nil, nil)
        }
        let key = cand.text
        if act {
            if let last = lastText, last.0 == key, Date().timeIntervalSince(last.1) < 2 {
                trace("dedupe: \(key)")
                return (nil, nil)
            }
            lastText = (key, Date())
        }

        let tty = ClipContext.terminalFrontTTY(frontBundleID: frontBundleID)
        let rows = cand.isAbsolute ? [] : await ClipContext.daemonRows(listen: listenAddress())
        let extra = extraBases
        let (res, snap) = await Task.detached {
            let snap = cand.isAbsolute
                ? ClipContext.Snapshot(bases: [], frontSession: nil, note: "absolute")
                : ClipContext.gather(frontTTY: tty, frontIsTerminal: frontIsTerminal, daemonRows: rows, extraBases: extra)
            let res = PathResolver().resolve(cand, bases: snap.bases)
            return (res, snap)
        }.value
        trace("\(cand.kind.rawValue) \(cand.path) line=\(cand.line.map(String.init) ?? "-") → \(res.matches.count) match(es) [\(snap.note)] bases=\(snap.bases.map(\.path))")
        for m in res.matches { trace("  \(m.path)  (\(m.base?.source ?? "absolute"))") }
        lastEvent = switch res.outcome {
        case .none: "No match for \(cand.displayName)"
        case .one: "\(cand.displayName) → \(res.matches[0].path)"
        case .several: "\(cand.displayName): \(res.matches.count) matches"
        }
        if act { perform(res) }
        return (res, snap)
    }

    func perform(_ res: Resolution) {
        let c = res.candidate
        switch res.outcome {
        case .none:
            if showNoMatch { hud.show(.noMatch(c)) }
        case .several:
            hud.show(.choose(c, res.matches))
        case .one:
            let m = res.matches[0]
            switch mode {
            case .open:
                let done = ClipActions.open(m, line: c.line, column: c.column, preferredEditor: lineEditor)
                hud.show(.opened(c, m, done))
            case .quickLook:
                ClipActions.quickLook(m.path)
                hud.show(.opened(c, m, .quickLook))
            case .ask:
                hud.show(.ask(c, m))
            }
        }
    }

    /// From the chooser / ask HUD.
    func open(_ m: PathMatch, for c: PathCandidate) {
        let done = ClipActions.open(m, line: c.line, column: c.column, preferredEditor: lineEditor)
        hud.show(.opened(c, m, done))
    }
}
