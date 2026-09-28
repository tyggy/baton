import AppKit
import SwiftUI

/// Settings scene: Status (every check, including optional permissions) and
/// Advanced (daemon address, logs, setup again).
struct SettingsView: View {
    var body: some View {
        TabView {
            Tab("Status", systemImage: "checkmark.seal") { StatusPane() }
            Tab("Advanced", systemImage: "gearshape.2") { AdvancedPane() }
        }
        .frame(width: 600, height: 620)
    }
}

struct StatusPane: View {
    @EnvironmentObject var daemon: DaemonController
    @EnvironmentObject var permissions: Permissions
    @EnvironmentObject var tailscale: TailscaleModel

    var body: some View {
        Form {
            Section("Baton") {
                StatusRow(title: "Running on this Mac",
                          reason: "Keeps your sessions available to your iPhone while you're logged in.",
                          status: daemonStatus) {
                    switch daemon.registration {
                    case .enabled: Button("Restart") { daemon.restart() }
                    case .requiresApproval: Button("Open Login Items") { daemon.openLoginItems() }
                    default: Button("Start") { Task { _ = await daemon.ensureRunning() } }
                    }
                }
            }
            Section("Permissions") {
                StatusRow(title: "Terminal", reason: "Brings a session's Terminal window to the front when you open it from your iPhone.",
                          status: permissions.automation) {
                    if permissions.automation.level != .ok {
                        Button("Ask") { permissions.refreshAutomation(ask: true) }
                    }
                    Button("Settings…") { SettingsLink.open(SettingsLink.automation) }
                }
                StatusRow(title: "Documents folder", reason: "Optional. Lets your iPhone open files and notes kept in Documents.",
                          status: permissions.documents) {
                    Button("Check") { permissions.refreshDocuments() }
                    Button("Settings…") { SettingsLink.open(SettingsLink.files) }
                }
                Toggle(isOn: Binding(
                    get: { permissions.notifications.level == .ok },
                    set: { on in
                        if on && permissions.notifications.detail == "Off" { permissions.requestNotifications() }
                        else { SettingsLink.open(SettingsLink.notifications) }
                    })) {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        StatusSymbol(level: permissions.notifications.level)
                        VStack(alignment: .leading, spacing: 2) {
                            Text("Notifications")
                            Text("Optional. Alerts on this Mac when Baton stops or a session needs you.")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
                StatusRow(title: "Accessibility", reason: "Not needed yet. A later version arranges Terminal windows for you.",
                          status: permissions.accessibility) { EmptyView() }
            }
            Section("Remote access") {
                let i = tailscale.info
                StatusRow(title: "Tailscale", reason: "Connects your iPhone to this Mac privately.",
                          status: i == nil ? .checking
                          : !i!.installed ? CheckStatus(level: .attention, detail: "Not installed")
                          : i!.running ? CheckStatus(level: .ok, detail: "Connected")
                          : CheckStatus(level: .attention, detail: i!.backendState ?? "Not connected")) {
                    if i?.installed == false { Button("Get Tailscale") { SettingsLink.open("https://tailscale.com/download/mac") } }
                }
                StatusRow(title: "Secure connections", reason: "HTTPS certificates for your tailnet.",
                          status: i == nil ? .checking
                          : i!.httpsEnabled ? CheckStatus(level: .ok, detail: "On")
                          : CheckStatus(level: .attention, detail: "Off")) {
                    if i?.httpsEnabled == false { Button("Open Tailscale Settings") { SettingsLink.open("https://login.tailscale.com/admin/dns") } }
                }
                StatusRow(title: "Away from home", reason: "Shares Baton with your Tailscale devices (tailscale serve).",
                          status: serveStatus(i)) { EmptyView() }
            }
            Section {
                HStack {
                    Spacer()
                    Button("Check Again") {
                        permissions.refreshAll(includeDocuments: true)
                        tailscale.refresh()
                        Task { await daemon.pollHealth() }
                    }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear {
            permissions.refreshAll(includeDocuments: true)
            tailscale.refresh()
        }
    }

    private var daemonStatus: CheckStatus {
        switch daemon.liveness {
        case .running(let h): CheckStatus(level: .ok, detail: "Running (v\(h.daemon ?? "?"))")
        case .stopped: CheckStatus(level: daemon.registration == .enabled ? .bad : .attention,
                                   detail: daemon.registration == .requiresApproval ? "Needs approval" : "Stopped")
        case .error(let e): CheckStatus(level: .bad, detail: e)
        case .unknown: .checking
        }
    }

    private func serveStatus(_ i: TailscaleInfo?) -> CheckStatus {
        guard let i else { return .checking }
        if i.serves(port: daemon.listenPort) { return CheckStatus(level: .ok, detail: "On") }
        if i.servePorts.isEmpty { return CheckStatus(level: .pending, detail: "Off") }
        return CheckStatus(level: .pending, detail: "Shares something else")
    }
}

struct AdvancedPane: View {
    @EnvironmentObject var daemon: DaemonController
    @Environment(\.openWindow) private var openWindow
    @State private var listen = ""
    @State private var upstream = ""

    var body: some View {
        Form {
            Section("Background service") {
                LabeledContent("Status", value: daemon.statusLine.replacingOccurrences(of: "Daemon: ", with: ""))
                LabeledContent("LaunchAgent", value: "\(daemon.label) — \(daemon.registration.label)")
                TextField("Listen address", text: $listen, prompt: Text(DaemonController.defaultListen))
                TextField("Upstream (Python server)", text: $upstream, prompt: Text(DaemonController.defaultUpstream))
                HStack {
                    Spacer()
                    Button("Apply and Restart") {
                        daemon.listen = listen.isEmpty ? DaemonController.defaultListen : listen
                        daemon.upstream = upstream.isEmpty ? DaemonController.defaultUpstream : upstream
                        if daemon.registration == .enabled { daemon.restart() } else { daemon.writeConfig() }
                    }
                    if daemon.registration == .enabled || daemon.registration == .requiresApproval {
                        Button("Stop") { daemon.unregister() }
                    } else {
                        Button("Start") { Task { _ = await daemon.ensureRunning() } }
                    }
                }
                if let e = daemon.lastError {
                    Text(e).font(.caption).foregroundStyle(.red)
                }
            }
            Section("Files") {
                LabeledContent("Config", value: DaemonController.configURL.path)
                LabeledContent("Logs") {
                    Button("Open Logs") {
                        try? FileManager.default.createDirectory(at: DaemonController.logsURL, withIntermediateDirectories: true)
                        NSWorkspace.shared.open(DaemonController.logsURL)
                    }
                }
            }
            Section("Setup") {
                LabeledContent("Setup assistant") {
                    Button("Show Setup Again…") {
                        NSApp.activate()
                        openWindow(id: "onboarding")
                    }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear {
            listen = daemon.listen
            upstream = daemon.upstream
        }
    }
}
