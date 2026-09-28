import AppKit
import SwiftUI

@main
struct BatonMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel.shared

    var body: some Scene {
        MenuBarExtra {
            MenuContent()
                .environmentObject(model)
                .environmentObject(model.daemon)
                .environmentObject(model.permissions)
                .environmentObject(model.tailscale)
        } label: {
            Image(systemName: model.menuSymbol)
        }
        .menuBarExtraStyle(.menu)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_: Notification) {
        MainActor.assumeIsolated {
            let m = AppModel.shared
            m.start()
            // `--show onboarding|pairing` opens a window at launch (used by
            // scripted checks and screenshots).
            let args = ProcessInfo.processInfo.arguments
            // `--daemon register|unregister`: scripted registration (checks, CI smoke).
            if let i = args.firstIndex(of: "--daemon"), i + 1 < args.count {
                args[i + 1] == "unregister" ? m.daemon.unregister() : m.daemon.register()
            }
            if let i = args.firstIndex(of: "--show"), i + 1 < args.count {
                args[i + 1] == "pairing" ? WindowManager.shared.showPairing() : WindowManager.shared.showOnboarding()
                // `--snapshot <png>`: render the window's own content (no
                // Screen Recording permission needed) once checks settle.
                if let j = args.firstIndex(of: "--snapshot"), j + 1 < args.count {
                    let id = args[i + 1] == "pairing" ? "pairing" : "onboarding"
                    let path = args[j + 1]
                    Task { @MainActor in
                        try? await Task.sleep(for: .seconds(6))
                        WindowManager.shared.snapshot(id: id, to: URL(fileURLWithPath: path))
                    }
                }
            } else if !UserDefaults.standard.bool(forKey: "onboarding.shown") {
                UserDefaults.standard.set(true, forKey: "onboarding.shown")
                WindowManager.shared.showOnboarding()
            }
        }
    }
}

@MainActor
final class AppModel: ObservableObject {
    static let shared = AppModel()
    let daemon = DaemonController()
    let permissions = Permissions()
    let tailscale = TailscaleModel()
    private var tsTimer: Timer?

    var menuSymbol: String {
        if case .running = daemon.liveness { return "point.3.connected.trianglepath.dotted" }
        return "point.3.filled.connected.trianglepath.dotted"
    }

    func start() {
        daemon.startPolling()
        permissions.refreshAll(includeDocuments: false)
        tailscale.refresh()
        tsTimer = Timer.scheduledTimer(withTimeInterval: 60, repeats: true) { _ in
            Task { @MainActor in
                AppModel.shared.tailscale.refresh()
                AppModel.shared.permissions.refreshAll(includeDocuments: false)
            }
        }
    }
}

/// Plain AppKit windows (an LSUIElement app has no window scenes to open at
/// launch); each hosts a SwiftUI view and brings the app forward.
@MainActor
final class WindowManager {
    static let shared = WindowManager()
    private var windows: [String: NSWindow] = [:]

    func showOnboarding() {
        show(id: "onboarding", title: "Baton Setup", size: NSSize(width: 640, height: 720)) {
            OnboardingView()
        }
        AppModel.shared.permissions.refreshAll(includeDocuments: true)
        AppModel.shared.tailscale.refresh()
    }

    func showPairing() {
        show(id: "pairing", title: "Pair iPhone", size: NSSize(width: 440, height: 600)) {
            PairingView()
        }
        AppModel.shared.tailscale.refresh()
    }

    func snapshot(id: String, to url: URL) {
        guard let v = windows[id]?.contentView,
              let rep = v.bitmapImageRepForCachingDisplay(in: v.bounds) else { return }
        v.cacheDisplay(in: v.bounds, to: rep)
        try? rep.representation(using: .png, properties: [:])?.write(to: url)
    }

    private func show<V: View>(id: String, title: String, size: NSSize, @ViewBuilder content: () -> V) {
        let m = AppModel.shared
        if let w = windows[id] {
            w.makeKeyAndOrderFront(nil)
            NSApp.activate()
            return
        }
        let root = content()
            .environmentObject(m)
            .environmentObject(m.daemon)
            .environmentObject(m.permissions)
            .environmentObject(m.tailscale)
        let w = NSWindow(
            contentRect: NSRect(origin: .zero, size: size),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered, defer: false)
        w.title = title
        w.isReleasedWhenClosed = false
        w.contentView = NSHostingView(rootView: root)
        w.center()
        windows[id] = w
        w.makeKeyAndOrderFront(nil)
        NSApp.activate()
    }
}
