import AppKit
import ScreenCaptureKit
import SwiftUI

@main
struct BatonMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel.shared

    var body: some Scene {
        MenuBarExtra {
            MenuContent().environments(model)
        } label: {
            Image(systemName: model.menuSymbol)
        }
        .menuBarExtraStyle(.menu)

        Window("Welcome to Baton", id: "onboarding") {
            SetupFlow()
                .environments(model)
                .containerBackground(.thickMaterial, for: .window)
        }
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentSize)
        .windowBackgroundDragBehavior(.enabled)
        .restorationBehavior(.disabled)
        .defaultPosition(.center)
        .defaultLaunchBehavior(LaunchOptions.openOnboardingAtLaunch ? .presented : .suppressed)

        Window("Pair iPhone", id: "pairing") {
            PairingView()
                .environments(model)
                .containerBackground(.thickMaterial, for: .window)
        }
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentSize)
        .windowBackgroundDragBehavior(.enabled)
        .restorationBehavior(.disabled)
        .defaultPosition(.center)
        .defaultLaunchBehavior(LaunchOptions.show == "pairing" ? .presented : .suppressed)

        Settings {
            SettingsView().environments(model)
        }
    }
}

extension View {
    func environments(_ m: AppModel) -> some View {
        environmentObject(m)
            .environmentObject(m.daemon)
            .environmentObject(m.permissions)
            .environmentObject(m.tailscale)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_: Notification) {
        switch LaunchOptions.appearance {
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        default: break
        }
    }

    func applicationDidFinishLaunching(_: Notification) {
        MainActor.assumeIsolated {
            let m = AppModel.shared
            m.start()
            switch LaunchOptions.daemonAction {
            case "register": m.daemon.register()
            case "unregister": m.daemon.unregister()
            default: break
            }
            if LaunchOptions.show == "settings" { Snapshots.showSettingsPreview() }
            if LaunchOptions.openOnboardingAtLaunch || LaunchOptions.show != nil { NSApp.activate() }
            if let path = LaunchOptions.snapshot {
                Task { @MainActor in
                    try? await Task.sleep(for: .seconds(4))
                    guard await Snapshots.makeKey(windowID: LaunchOptions.show ?? "onboarding") else {
                        FileHandle.standardError.write(Data("snapshot: window never became key; not writing an inactive-looking image\n".utf8))
                        exit(3)
                    }
                    await Snapshots.write(windowID: LaunchOptions.show ?? "onboarding", to: URL(fileURLWithPath: path))
                    NSApp.terminate(nil)
                }
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

    static let menuSymbolRunning = "point.3.connected.trianglepath.dotted"
    static let menuSymbolStopped = "point.3.filled.connected.trianglepath.dotted"

    var menuSymbol: String {
        if case .running = daemon.liveness { return Self.menuSymbolRunning }
        return Self.menuSymbolStopped
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

/// `--snapshot`: render a window's content to PNG from inside the app (no
/// Screen Recording permission). Materials don't draw through
/// cacheDisplay, so the window background colour is painted underneath.
@MainActor
enum Snapshots {
    private static var preview: NSWindow?

    /// Snapshot mode only: force activation (the shell running the snapshot
    /// is frontmost) and wait until the window is really key, so controls
    /// render as the user sees them. Retries; false if it never happens.
    static func makeKey(windowID: String) async -> Bool {
        for _ in 0..<10 {
            if let w = NSApp.windows.first(where: { $0.identifier?.rawValue.hasPrefix(windowID) == true && $0.isVisible }) {
                NSApp.perform(NSSelectorFromString("activateIgnoringOtherApps:"), with: true)
                w.makeKeyAndOrderFront(nil)
                try? await Task.sleep(for: .milliseconds(700))
                if NSApp.isActive && w.isKeyWindow {
                    try? await Task.sleep(for: .milliseconds(600)) // let controls redraw as key
                    if NSApp.isActive && w.isKeyWindow { return true }
                }
            } else {
                try? await Task.sleep(for: .milliseconds(500))
            }
        }
        return false
    }

    /// Settings content in a plain window (the Settings scene can't be opened
    /// programmatically at launch).
    static func showSettingsPreview() {
        let m = AppModel.shared
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 620),
                         styleMask: [.titled, .closable], backing: .buffered, defer: false)
        w.identifier = NSUserInterfaceItemIdentifier("settings")
        w.title = "Settings"
        w.contentView = NSHostingView(rootView: SettingsView().environments(m))
        w.center()
        w.makeKeyAndOrderFront(nil)
        preview = w
    }

    /// Captures the real window (glass and materials included) through
    /// ScreenCaptureKit's current-process content, which needs no Screen
    /// Recording permission. Falls back to cacheDisplay.
    static func write(windowID: String, to url: URL) async {
        guard let w = NSApp.windows.first(where: { $0.identifier?.rawValue.hasPrefix(windowID) == true && $0.isVisible }) else {
            FileHandle.standardError.write(Data("snapshot: no window \(windowID)\n".utf8))
            return
        }
        w.makeKeyAndOrderFront(nil)
        do {
            let content = try await SCShareableContent.currentProcess
            guard let scw = content.windows.first(where: { $0.windowID == CGWindowID(w.windowNumber) }) else {
                throw NSError(domain: "snapshot", code: 1)
            }
            let filter = SCContentFilter(desktopIndependentWindow: scw)
            let cfg = SCStreamConfiguration()
            let scale = w.backingScaleFactor
            cfg.width = Int(filter.contentRect.width * scale)
            cfg.height = Int(filter.contentRect.height * scale)
            cfg.showsCursor = false
            cfg.ignoreShadowsSingleWindow = true
            let cg = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: cfg)
            let rep = NSBitmapImageRep(cgImage: cg)
            try rep.representation(using: .png, properties: [:])?.write(to: url)
            return
        } catch {
            FileHandle.standardError.write(Data("snapshot: SCK failed (\(error)); using cacheDisplay\n".utf8))
        }
        guard let v = w.contentView, let rep = v.bitmapImageRepForCachingDisplay(in: v.bounds) else { return }
        v.cacheDisplay(in: v.bounds, to: rep)
        try? rep.representation(using: .png, properties: [:])?.write(to: url)
    }
}
