import AppKit
import SwiftUI

// MARK: 1 · Welcome

struct WelcomePage: View {
    @EnvironmentObject var daemon: DaemonController
    let pinned: Bool
    let onDone: () -> Void

    enum Phase: Equatable { case idle, working, needsApproval, failed(String), done }
    @State private var phase: Phase = .idle

    var body: some View {
        SetupPage(
            symbol: "macbook.and.iphone",
            done: phase == .done,
            title: phase == .needsApproval ? "Allow Baton in the background" : "Welcome to Baton",
            message: message,
            primaryTitle: primaryTitle,
            busy: phase == .working,
            primary: primary
        ) {
            if case .failed(let details) = phase {
                DetailsDisclosure(text: details)
            }
        }
        .task(id: phase) {
            // While waiting for Login Items approval, notice it by itself.
            guard phase == .needsApproval else { return }
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                daemon.refreshRegistration()
                if daemon.registration == .enabled { start(); return }
            }
        }
    }

    private var message: String {
        switch phase {
        case .needsApproval: "Turn on Baton in Login Items so it can keep your sessions available."
        case .failed: "Baton couldn't finish setting up on this Mac."
        case .done: "Baton is running on this Mac."
        default: "Use your Claude Code and Codex sessions from your iPhone."
        }
    }

    private var primaryTitle: String {
        switch phase {
        case .working: "Setting Up…"
        case .needsApproval: "Open Login Items"
        case .failed: "Try Again"
        default: "Continue"
        }
    }

    private func primary() {
        if phase == .needsApproval { daemon.openLoginItems() } else { start() }
    }

    private func start() {
        guard !pinned else { return }
        phase = .working
        Task {
            switch await daemon.ensureRunning() {
            case .running: phase = .done; onDone()
            case .needsApproval: phase = .needsApproval
            case .failed(let d): phase = .failed(d)
            }
        }
    }
}

// MARK: 2 · Terminal pre-alert

/// Pre-alert before the system Automation prompt: exactly one button,
/// "Continue", which triggers the alert. A denial gets its own screen.
struct TerminalPage: View {
    @EnvironmentObject var permissions: Permissions
    let pinned: Bool
    let onDone: () -> Void
    let onSkip: () -> Void
    @State private var asked = false

    /// Pinned (screenshots) always shows the pre-alert, whatever the live state.
    private var level: Level { pinned ? .attention : permissions.automation.level }

    var body: some View {
        Group {
            if level == .bad {
                SetupPage(
                    symbol: "terminal",
                    symbolColor: .secondary,
                    title: "Sessions won't come to the front",
                    message: "When you open a session from your iPhone, its Terminal window stays where it is. You can turn this on in System Settings at any time.",
                    primaryTitle: "Open System Settings",
                    primary: { SettingsLink.open(SettingsLink.automation) }
                ) {
                    Button("Continue Without It", action: onSkip)
                }
            } else {
                SetupPage(
                    symbol: "terminal",
                    done: level == .ok,
                    title: "Let Baton open your sessions in Terminal",
                    message: "When you pick a session on your iPhone, Baton brings its Terminal window to the front.",
                    primaryTitle: "Continue",
                    busy: asked && level == .checking,
                    primary: {
                        guard !pinned else { return }
                        asked = true
                        permissions.refreshAutomation(ask: true)
                    }
                ) { EmptyView() }
            }
        }
        .onAppear { if !pinned { permissions.refreshAutomation(ask: false) } }
        .onChange(of: level) { _, new in if new == .ok { onDone() } }
        .task {
            // Notice a change made in System Settings while this screen is up.
            guard !pinned else { return }
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                if permissions.automation.level != .checking { permissions.refreshAutomation(ask: false) }
            }
        }
    }
}

// MARK: 3 · Pair

struct PairPage: View {
    @EnvironmentObject var tailscale: TailscaleModel
    @StateObject private var watcher = PairingWatcher()
    let pinned: Bool
    let onDone: () -> Void
    let onNext: () -> Void
    @State private var copied = false

    private var token: String? { Pairing.loadToken() }
    private var payload: String? { token.flatMap { Pairing.payload(ts: tailscale.info, token: $0) } }

    var body: some View {
        SetupPage(
            symbol: nil,
            title: watcher.connected ? "Your iPhone is connected" : "Scan with your iPhone",
            message: watcher.connected ? "You can use your sessions from your iPhone now." : "This code links Baton on your iPhone to this Mac.",
            primaryTitle: watcher.connected ? "Continue" : "Done",
            primary: onNext
        ) {
            VStack(spacing: 16) {
                if watcher.connected {
                    StepSymbol(symbol: "iphone", done: true, size: 96)
                        .frame(height: 236)
                } else if let payload {
                    QRCard(payload: payload)
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text("Waiting for your iPhone…")
                    }
                    .font(.callout)
                    .foregroundStyle(.secondary)
                } else if token == nil {
                    notice("Baton needs an access token on this Mac first.", systemImage: "key")
                } else {
                    ProgressView().frame(height: 236)
                }
            }
        } links: {
            if !watcher.connected, let payload {
                Button(copied ? "Copied" : "Copy Pairing Code") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(payload, forType: .string)
                    copied = true
                }
            }
        }
        .onAppear {
            tailscale.refresh()
            if !pinned { watcher.start() }
        }
        .onDisappear { watcher.stop() }
        .onChange(of: watcher.connected) { _, c in if c { onDone() } }
    }

    private func notice(_ text: String, systemImage: String) -> some View {
        Label(text, systemImage: systemImage)
            .foregroundStyle(.secondary)
            .frame(height: 236)
    }
}

/// The QR code on a white rounded card with a phone glyph.
struct QRCard: View {
    let payload: String

    var body: some View {
        VStack(spacing: 12) {
            if let img = Pairing.qrImage(payload) {
                Image(nsImage: img)
                    .interpolation(.none)
                    .resizable()
                    .frame(width: 184, height: 184)
                    .accessibilityLabel("Pairing code")
            }
            Label("Open Baton on your iPhone and scan", systemImage: "iphone")
                .font(.callout)
                .foregroundStyle(Color.black.opacity(0.65))
        }
        .padding(18)
        .background(Color.white, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 22, style: .continuous).strokeBorder(.separator))
        .environment(\.colorScheme, .light)
    }
}

// MARK: 4 · Remote access

struct RemotePage: View {
    @EnvironmentObject var tailscale: TailscaleModel
    @EnvironmentObject var daemon: DaemonController
    let pinned: Bool
    let onDone: () -> Void
    let onLater: () -> Void
    @State private var enabling = false
    @State private var enableError: String?

    enum Phase: Equatable { case checking, install, signIn, secure, share, ready }

    private var phase: Phase {
        guard let i = tailscale.info else { return .checking }
        if !i.installed { return .install }
        if i.error != nil || !i.running { return .signIn }
        if !i.httpsEnabled { return .secure }
        if !i.serves(port: daemon.listenPort) { return .share }
        return .ready
    }

    var body: some View {
        SetupPage(
            symbol: "network",
            done: phase == .ready,
            title: "Reach your Mac away from home",
            message: message,
            primaryTitle: primaryTitle,
            busy: phase == .checking || enabling,
            primary: primary
        ) {
            VStack(spacing: 10) {
                if phase == .share {
                    Text(sharesSomethingElse
                         ? "This replaces what this Mac currently shares with your Tailscale devices."
                         : "Only your own devices on Tailscale can reach it. You can turn this off at any time.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                if let enableError {
                    Text("Remote access couldn't be turned on.").font(.callout)
                    DetailsDisclosure(text: enableError)
                }
            }
        } links: {
            if phase != .ready {
                Button("Set Up Later", action: onLater)
                if enableError == nil { DetailsDisclosure(text: details) }
            }
        }
        .onAppear { tailscale.refresh() }
        .onChange(of: phase) { _, p in if p == .ready { onDone() } }
        .task {
            // Recheck while the user installs or signs in to Tailscale.
            guard !pinned else { return }
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(3))
                if phase != .ready && !enabling { tailscale.refresh() }
            }
        }
    }

    private var sharesSomethingElse: Bool { !(tailscale.info?.servePorts.isEmpty ?? true) }

    private var message: String {
        switch phase {
        case .checking: "Checking your connection…"
        case .install: "Baton connects through Tailscale, a free app that links your own devices privately."
        case .signIn: "Open Tailscale and sign in to continue."
        case .secure: "Turn on secure connections for your Tailscale network. It's one switch in Tailscale's settings."
        case .share: "Let your iPhone reach this Mac from anywhere, over your private Tailscale network."
        case .ready: "Your iPhone can reach this Mac from anywhere."
        }
    }

    private var primaryTitle: String {
        switch phase {
        case .checking: "Checking…"
        case .install: "Get Tailscale"
        case .signIn: "Open Tailscale"
        case .secure: "Open Tailscale Settings"
        case .share: enabling ? "Turning On…" : "Turn On Remote Access"
        case .ready: "Continue"
        }
    }

    private func primary() {
        switch phase {
        case .checking: break
        case .install: SettingsLink.open("https://tailscale.com/download/mac")
        case .signIn:
            NSWorkspace.shared.open(URL(fileURLWithPath: "/Applications/Tailscale.app"))
        case .secure: SettingsLink.open("https://login.tailscale.com/admin/dns")
        case .share:
            // Changes the user's Tailscale config — only ever on this click.
            guard !pinned else { return }
            enabling = true
            enableError = nil
            Task {
                enableError = await tailscale.enableServe(port: daemon.listenPort)
                enabling = false
            }
        case .ready: onLater()
        }
    }

    private var details: String {
        guard let i = tailscale.info else { return "tailscale: checking" }
        return """
        cli: \(i.cliPath ?? "not found")
        BackendState: \(i.backendState ?? "-")\(i.error.map { "\nerror: \($0)" } ?? "")
        CertDomains: \(i.certDomains.isEmpty ? "none (HTTPS off)" : "\(i.certDomains.count)")
        DNSName: \(i.dnsName ?? "-")
        serve: \(i.servePorts.isEmpty ? "not configured" : i.servePorts.joined(separator: ", "))
        turn on runs: tailscale \(TailscaleModel.serveArgs(port: daemon.listenPort).joined(separator: " "))
        """
    }
}

// MARK: Finished

struct FinishedPage: View {
    @EnvironmentObject var model: AppModel
    let pinned: Bool
    let onClose: () -> Void
    @State private var appeared = false

    var body: some View {
        SetupPage(
            symbol: "checkmark.circle.fill",
            symbolColor: .green,
            title: "You're all set",
            message: "Baton is in the menu bar. Click it any time to check on things or pair another iPhone.",
            primaryTitle: "Done",
            primary: onClose
        ) {
            MenuBarHint(symbol: model.menuSymbol)
                .padding(.top, 4)
        } links: {
            EmptyView()
        }
        .task {
            // A brief confirmation, then back to the menu bar.
            guard !pinned else { return }
            try? await Task.sleep(for: .seconds(4))
            onClose()
        }
    }
}

/// A small menu-bar picture pointing at Baton's icon.
struct MenuBarHint: View {
    let symbol: String
    @State private var pulse = false

    var body: some View {
        VStack(spacing: 6) {
            HStack(spacing: 16) {
                Image(systemName: "wifi")
                Image(systemName: symbol)
                    .foregroundStyle(Color.accentColor)
                    .padding(.horizontal, 7).padding(.vertical, 3)
                    .background(Color.accentColor.opacity(0.18), in: Capsule())
                Image(systemName: "battery.75percent")
                Text("9:41").monospacedDigit()
            }
            .font(.system(size: 15))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 18).padding(.vertical, 9)
            .glassEffect(.regular, in: Capsule())
            Image(systemName: "arrow.up")
                .font(.title2.weight(.semibold))
                .foregroundStyle(Color.accentColor)
                .symbolEffect(.bounce.up, options: .repeat(2), value: pulse)
                .offset(x: -22)
        }
        .onAppear { pulse = true }
    }
}
