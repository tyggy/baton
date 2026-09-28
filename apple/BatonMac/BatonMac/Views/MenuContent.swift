import AppKit
import SwiftUI

struct MenuContent: View {
    @EnvironmentObject var daemon: DaemonController
    @EnvironmentObject var permissions: Permissions
    @EnvironmentObject var tailscale: TailscaleModel

    var body: some View {
        Text(daemon.statusLine)
        if daemon.registration != .enabled {
            Text("LaunchAgent: \(daemon.registration.label)")
        }
        Text(permissions.missingCount == 0
             ? "Permissions: all set"
             : "Permissions: \(permissions.missingCount) need attention")
        Text(tailscale.summary)
        Divider()
        Button("Open Onboarding…") { WindowManager.shared.showOnboarding() }
            .keyboardShortcut(",")
        Button("Pair iPhone…") { WindowManager.shared.showPairing() }
        Button("Open Logs") {
            let dir = DaemonController.logsURL
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            NSWorkspace.shared.open(dir)
        }
        Divider()
        if daemon.registration == .enabled {
            Button("Restart Daemon") { daemon.restart() }
        } else {
            Button("Start Daemon") { daemon.register() }
        }
        Divider()
        Button("Quit Baton") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}
