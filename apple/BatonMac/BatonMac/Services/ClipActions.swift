import AppKit
import PathDetect
import Quartz

/// What happens to a resolved path. Only NSWorkspace / Quick Look / Finder;
/// the copied text is never run. Anything whose default "open" would run
/// code (apps, `.command`, installers, executables) is revealed instead.
@MainActor
enum ClipActions {
    struct Editor: Identifiable, Hashable {
        var id: String { bundleID }
        var bundleID: String
        var name: String
    }

    /// Editors that can open a file at a line, and how.
    static let lineEditors: [Editor] = [
        Editor(bundleID: "com.microsoft.VSCode", name: "Visual Studio Code"),
        Editor(bundleID: "com.todesktop.230313mzl4w4u92", name: "Cursor"),
        Editor(bundleID: "dev.zed.Zed", name: "Zed"),
        Editor(bundleID: "com.apple.dt.Xcode", name: "Xcode"),
        Editor(bundleID: "com.barebones.bbedit", name: "BBEdit"),
        Editor(bundleID: "com.sublimetext.4", name: "Sublime Text"),
        Editor(bundleID: "com.sublimetext.3", name: "Sublime Text 3"),
    ]

    static var installedEditors: [Editor] {
        lineEditors.filter { NSWorkspace.shared.urlForApplication(withBundleIdentifier: $0.bundleID) != nil }
    }

    /// Apps whose "open" executes the document.
    static let runnerApps: Set<String> = [
        "com.apple.Terminal", "com.googlecode.iterm2", "com.mitchellh.ghostty", "dev.warp.Warp-Stable",
        "com.github.wez.wezterm", "net.kovidgoyal.kitty", "org.alacritty",
        "com.apple.installer", "com.apple.ScriptEditor.id.cocoa-applet-template", "com.apple.AutomatorRunner",
        "com.apple.automator.xpc.runner", "com.apple.Automator", "com.apple.shortcuts",
        "org.python.PythonLauncher", "com.apple.JarLauncher", "com.apple.systempreferences",
        "com.apple.archiveutility", "com.apple.DiskImageMounter", "com.apple.appstoreagent",
    ]
    static let runnerExtensions: Set<String> = [
        "app", "command", "tool", "terminal", "workflow", "action", "shortcut", "pkg", "mpkg",
        "dmg", "jar", "webloc", "inetloc", "url", "fileloc", "prefpane", "saver", "kext",
        "osax", "plugin", "bundle", "framework", "dylib", "mobileconfig",
    ]

    /// True when opening `url` in its default app can't run it.
    static func safeToOpen(_ url: URL) -> Bool {
        let ext = url.pathExtension.lowercased()
        if runnerExtensions.contains(ext) { return false }
        let rv = try? url.resourceValues(forKeys: [.isDirectoryKey, .isPackageKey, .isExecutableKey])
        if rv?.isPackage == true { return false }
        if rv?.isDirectory != true, rv?.isExecutable == true, ext.isEmpty { return false }
        if let app = NSWorkspace.shared.urlForApplication(toOpen: url),
           let id = Bundle(url: app)?.bundleIdentifier, runnerApps.contains(id) { return false }
        return true
    }

    enum Done: Equatable { case opened(String), revealed, quickLook }

    /// Open: directories reveal in Finder; files go to the default app, at
    /// the line when the app is a known editor (or `preferredEditor` is set).
    @discardableResult
    static func open(_ m: PathMatch, line: Int?, column: Int?, preferredEditor: String?) -> Done {
        let url = URL(fileURLWithPath: m.path)
        if m.isDirectory && !isPackage(url) {
            // Directory: reveal it (selected in its parent) in Finder.
            NSWorkspace.shared.activateFileViewerSelecting([url])
            return .revealed
        }
        guard safeToOpen(url) else {
            NSWorkspace.shared.activateFileViewerSelecting([url])
            return .revealed
        }
        let defaultApp = NSWorkspace.shared.urlForApplication(toOpen: url)
        let defaultID = defaultApp.flatMap { Bundle(url: $0)?.bundleIdentifier }
        let editorID = preferredEditor.flatMap { id in
            NSWorkspace.shared.urlForApplication(withBundleIdentifier: id) == nil ? nil : id
        } ?? defaultID

        if let line, let editorID, openAtLine(url, line: line, column: column, editor: editorID) {
            return .opened(editorName(editorID))
        }
        if let preferredEditor, preferredEditor == editorID,
           let app = NSWorkspace.shared.urlForApplication(withBundleIdentifier: preferredEditor) {
            NSWorkspace.shared.open([url], withApplicationAt: app, configuration: NSWorkspace.OpenConfiguration())
            return .opened(editorName(preferredEditor))
        }
        NSWorkspace.shared.open(url)
        return .opened(defaultApp.map { FileManager.default.displayName(atPath: $0.path).replacingOccurrences(of: ".app", with: "") } ?? "default app")
    }

    static func editorName(_ id: String) -> String {
        lineEditors.first { $0.bundleID == id }?.name
            ?? NSWorkspace.shared.urlForApplication(withBundleIdentifier: id).map {
                FileManager.default.displayName(atPath: $0.path).replacingOccurrences(of: ".app", with: "")
            } ?? id
    }

    /// URL schemes where the editor has one (no process spawned); Xcode and
    /// Sublime only have their CLIs (`xed`, `subl`), run with an argument
    /// array — the path is data, never a shell string.
    static func openAtLine(_ url: URL, line: Int, column: Int?, editor: String) -> Bool {
        let path = url.path
        let loc = column.map { "\(line):\($0)" } ?? "\(line)"
        func scheme(_ s: String) -> Bool {
            var c = URLComponents()
            c.scheme = s
            c.host = "file"
            c.path = path + ":" + loc
            guard let u = c.url else { return false }
            return NSWorkspace.shared.open(u)
        }
        switch editor {
        case "com.microsoft.VSCode": return scheme("vscode")
        case "com.todesktop.230313mzl4w4u92": return scheme("cursor")
        case "dev.zed.Zed": return scheme("zed")
        case "com.barebones.bbedit":
            var c = URLComponents()
            c.scheme = "x-bbedit"
            c.host = "open"
            c.queryItems = [URLQueryItem(name: "url", value: url.absoluteString), URLQueryItem(name: "line", value: "\(line)")]
            guard let u = c.url else { return false }
            return NSWorkspace.shared.open(u)
        case "com.apple.dt.Xcode":
            return spawn("/usr/bin/xed", ["-l", "\(line)", path])
        case "com.sublimetext.4", "com.sublimetext.3":
            guard let app = NSWorkspace.shared.urlForApplication(withBundleIdentifier: editor) else { return false }
            let subl = app.appendingPathComponent("Contents/SharedSupport/bin/subl").path
            return spawn(subl, [path + ":" + loc])
        default: return false
        }
    }

    private static func spawn(_ exe: String, _ args: [String]) -> Bool {
        guard FileManager.default.isExecutableFile(atPath: exe) else { return false }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: exe)
        p.arguments = args
        p.standardInput = FileHandle.nullDevice
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        return (try? p.run()) != nil
    }

    static func isPackage(_ url: URL) -> Bool {
        (try? url.resourceValues(forKeys: [.isPackageKey]))?.isPackage == true
    }

    static func reveal(_ path: String) {
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    static func quickLook(_ path: String) {
        QuickLookController.shared.show(URL(fileURLWithPath: path))
    }
}

/// Quick Look panel for a single file. A menu-bar app has no document
/// window in the responder chain, so it activates and drives the shared
/// panel as its data source directly.
@MainActor
final class QuickLookController: NSObject, @preconcurrency QLPreviewPanelDataSource, QLPreviewPanelDelegate {
    static let shared = QuickLookController()
    private var url: URL?

    func show(_ url: URL) {
        self.url = url
        NSApp.activate()
        guard let panel = QLPreviewPanel.shared() else { return }
        panel.dataSource = self
        panel.delegate = self
        panel.reloadData()
        panel.makeKeyAndOrderFront(nil)
    }

    func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int { url == nil ? 0 : 1 }

    func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> (any QLPreviewItem)! {
        url as NSURL?
    }
}
