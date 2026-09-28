import AppKit
import SwiftUI

struct MenuContent: View {
    @EnvironmentObject var daemon: DaemonController
    @EnvironmentObject var permissions: Permissions
    @EnvironmentObject var tailscale: TailscaleModel
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        Text(daemon.statusLine)
        Text(permissions.requiredMissing ? "Terminal access: needed" : "Terminal access: allowed")
        Text(tailscale.summary)
        Divider()
        Button("Pair iPhone…") { NSApp.activate(); openWindow(id: "pairing") }
        Button("Show Setup Again…") { NSApp.activate(); openWindow(id: "onboarding") }
        Button("Settings…") { NSApp.activate(); openSettings() }
            .keyboardShortcut(",")
        Button("Open Logs") {
            let dir = DaemonController.logsURL
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            NSWorkspace.shared.open(dir)
        }
        Divider()
        if daemon.registration == .enabled {
            Button("Restart Baton Service") { daemon.restart() }
        } else {
            Button("Start Baton Service") { Task { _ = await daemon.ensureRunning() } }
        }
        Divider()
        Button("Quit Baton") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}
