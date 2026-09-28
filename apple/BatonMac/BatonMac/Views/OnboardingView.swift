import AppKit
import SwiftUI

struct OnboardingView: View {
    @EnvironmentObject var daemon: DaemonController
    @EnvironmentObject var permissions: Permissions
    @EnvironmentObject var tailscale: TailscaleModel

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                header
                section("Daemon") { daemonRows }
                section("Permissions") { permissionRows }
                section("Remote access (Tailscale)") { tailscaleRows }
                HStack {
                    Spacer()
                    Button("Recheck all") {
                        permissions.refreshAll(includeDocuments: true)
                        tailscale.refresh()
                        Task { await daemon.pollHealth() }
                    }
                    Button("Pair iPhone…") { WindowManager.shared.showPairing() }
                        .buttonStyle(.borderedProminent)
                }
            }
            .padding(24)
        }
        .frame(minWidth: 560, minHeight: 520)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Set up Baton").font(.largeTitle.bold())
            Text("Baton runs a small background service that lets your iPhone see and drive your Claude Code and Codex sessions. macOS asks for a few permissions first; each row says why.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func section<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title.uppercased()).font(.caption.bold()).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 0) { content() }
                .padding(.horizontal, 14).padding(.vertical, 6)
                .background(RoundedRectangle(cornerRadius: 10).fill(.quaternary.opacity(0.5)))
        }
    }

    // MARK: daemon

    private var daemonStatus: CheckStatus {
        switch daemon.liveness {
        case .running(let h):
            CheckStatus(level: .ok, detail: "running v\(h.daemon ?? "?") (api \(h.apiVersion.map(String.init) ?? "?"), release \(h.release ?? "?"), pid \(h.pid.map(String.init) ?? "?")) on \(daemon.listen)")
        case .stopped:
            CheckStatus(level: daemon.registration == .enabled ? .bad : .attention,
                        detail: "not answering on \(daemon.listen) — LaunchAgent \(daemon.registration.label)")
        case .error(let e): CheckStatus(level: .bad, detail: e)
        case .unknown: .checking
        }
    }

    @State private var listenField = ""
    @State private var upstreamField = ""

    @ViewBuilder private var daemonRows: some View {
        RequirementRow(
            title: "Background service",
            reason: "Runs `baton serve` at login under Baton's identity, so macOS attributes its permission prompts to Baton instead of silently blocking it.",
            status: daemonStatus
        ) {
            switch daemon.registration {
            case .enabled:
                Button("Restart") { daemon.restart() }
                Button("Stop") { daemon.unregister() }
            case .requiresApproval:
                Button("Open Login Items") { daemon.openLoginItems() }
                Button("Stop") { daemon.unregister() }
            default:
                Button("Start") { daemon.register() }
            }
        }
        if let e = daemon.lastError {
            Text(e).font(.caption).foregroundStyle(.red).padding(.leading, 22)
        }
        Divider()
        HStack(spacing: 8) {
            Text("Listen").frame(width: 60, alignment: .leading)
            TextField(DaemonController.defaultListen, text: $listenField).frame(width: 150)
            Text("Upstream")
            TextField(DaemonController.defaultUpstream, text: $upstreamField)
            Button("Apply") {
                daemon.listen = listenField.isEmpty ? DaemonController.defaultListen : listenField
                daemon.upstream = upstreamField.isEmpty ? DaemonController.defaultUpstream : upstreamField
                if daemon.registration == .enabled { daemon.restart() } else { daemon.writeConfig() }
            }
        }
        .font(.callout)
        .controlSize(.small)
        .padding(.vertical, 8)
        .onAppear {
            listenField = daemon.listen
            upstreamField = daemon.upstream
        }
    }

    // MARK: permissions

    @ViewBuilder private var permissionRows: some View {
        RequirementRow(
            title: "Automation → Terminal",
            reason: "Opening or resuming a session from your iPhone brings its Terminal tab to the front. That needs permission to send Apple Events to Terminal.",
            status: permissions.automation
        ) {
            Button("Request") { permissions.refreshAutomation(ask: true) }
            Button("Settings") { SettingsLink.open(SettingsLink.automation) }
        }
        Divider()
        RequirementRow(
            title: "Files & Folders → Documents",
            reason: "Only needed if you open file links or a knowledge base that lives in ~/Documents from your iPhone. Session transcripts don't need it.",
            status: permissions.documents
        ) {
            Button("Check") { permissions.refreshDocuments() }
            Button("Settings") { SettingsLink.open(SettingsLink.files) }
        }
        Divider()
        RequirementRow(
            title: "Accessibility",
            reason: "Optional for now. Later versions arrange Terminal windows for you, which needs Accessibility.",
            status: permissions.accessibility
        ) {
            Button("Request") { permissions.requestAccessibility() }
            Button("Settings") { SettingsLink.open(SettingsLink.accessibility) }
        }
        Divider()
        RequirementRow(
            title: "Notifications",
            reason: "Lets Baton tell you on this Mac when the service stops or a session needs you.",
            status: permissions.notifications
        ) {
            Button("Request") { permissions.requestNotifications() }
            Button("Settings") { SettingsLink.open(SettingsLink.notifications) }
        }
    }

    // MARK: tailscale

    @ViewBuilder private var tailscaleRows: some View {
        let i = tailscale.info
        RequirementRow(
            title: "Tailscale installed",
            reason: "Your iPhone reaches this Mac through your own Tailscale network. Nothing goes through our servers.",
            status: i == nil ? .checking
                : i!.installed ? CheckStatus(level: .ok, detail: i!.cliPath ?? "")
                : CheckStatus(level: .bad, detail: "install from tailscale.com/download/mac")
        ) {
            if i?.installed == false {
                Button("Download") { SettingsLink.open("https://tailscale.com/download/mac") }
            }
        }
        Divider()
        RequirementRow(
            title: "Connected",
            reason: "Tailscale must be logged in and running on this Mac.",
            status: i == nil ? .checking
                : i!.running ? CheckStatus(level: .ok, detail: "BackendState Running — \(i!.dnsName ?? "?")")
                : CheckStatus(level: .bad, detail: i!.error ?? "BackendState \(i!.backendState ?? "unknown")")
        ) { EmptyView() }
        Divider()
        RequirementRow(
            title: "HTTPS certificates",
            reason: "The iPhone app talks HTTPS to <mac>.<tailnet>.ts.net. Enable HTTPS in the Tailscale admin console (DNS → HTTPS Certificates).",
            status: i == nil ? .checking
                : i!.httpsEnabled ? CheckStatus(level: .ok, detail: "enabled (\(i!.certDomains.count) cert domain\(i!.certDomains.count == 1 ? "" : "s"))")
                : CheckStatus(level: .bad, detail: "not enabled")
        ) {
            if i?.httpsEnabled == false {
                Button("Admin console") { SettingsLink.open("https://login.tailscale.com/admin/dns") }
            }
        }
        Divider()
        serveRow(i)
    }

    @ViewBuilder private func serveRow(_ i: TailscaleInfo?) -> some View {
        let port = daemon.listenPort
        let cmd = "tailscale serve --bg --https=443 http://127.0.0.1:\(port)"
        let status: CheckStatus = {
            guard let i else { return .checking }
            if i.serves(port: port) { return CheckStatus(level: .ok, detail: "serving http://127.0.0.1:\(port)") }
            if i.servePorts.isEmpty { return CheckStatus(level: .attention, detail: "not exposed — run: \(cmd)") }
            return CheckStatus(level: .attention, detail: "serve points at \(i.servePorts.joined(separator: ", ")), not this daemon (:\(port)). To switch: \(cmd)")
        }()
        RequirementRow(
            title: "tailscale serve",
            reason: "Exposes the daemon on your tailnet over HTTPS. Baton only checks this; it never changes your Tailscale settings.",
            status: status
        ) {
            Button("Copy command") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(cmd, forType: .string)
            }
        }
    }
}
