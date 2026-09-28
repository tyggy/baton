import AppKit
import PathDetect
import SwiftUI

/// Small non-activating panel at the top right: what a copied path opened,
/// a chooser when several files match, or a brief "no match". Never takes
/// focus from the terminal; hovering keeps it up.
@MainActor
final class ClipHUD {
    enum Content {
        case opened(PathCandidate, PathMatch, ClipActions.Done)
        case ask(PathCandidate, PathMatch)
        case choose(PathCandidate, [PathMatch])
        case noMatch(PathCandidate)

        var lifetime: TimeInterval {
            switch self {
            case .opened: 3
            case .noMatch: 2
            case .ask: 8
            case .choose: 12
            }
        }
    }

    private var panel: HUDPanel?
    private var dismissTask: Task<Void, Never>?
    private let hover = HoverState()

    func show(_ content: Content, persistent: Bool = false) {
        let p = panel ?? makePanel()
        panel = p
        let host = NSHostingView(rootView: ClipHUDView(content: content, hover: hover, close: { [weak self] in self?.hide() })
            .environmentObject(AppModel.shared.clip))
        host.sizingOptions = [.preferredContentSize]
        p.contentView = host
        let size = host.fittingSize
        let screen = NSScreen.screens.first { $0.frame.contains(NSEvent.mouseLocation) } ?? NSScreen.main
        let vf = screen?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        p.setFrame(NSRect(x: vf.maxX - size.width - 14, y: vf.maxY - size.height - 10,
                          width: size.width, height: size.height), display: true)
        p.alphaValue = 1
        p.orderFrontRegardless()
        dismissTask?.cancel()
        guard !persistent else { return }
        let life = content.lifetime
        dismissTask = Task { [weak self] in
            var left = life
            while left > 0 {
                try? await Task.sleep(for: .milliseconds(250))
                if Task.isCancelled { return }
                if self?.hover.inside != true { left -= 0.25 }
            }
            self?.hide()
        }
    }

    func hide() {
        dismissTask?.cancel()
        guard let p = panel else { return }
        NSAnimationContext.runAnimationGroup({ ctx in
            ctx.duration = 0.2
            p.animator().alphaValue = 0
        }, completionHandler: {
            MainActor.assumeIsolated { p.orderOut(nil) }
        })
    }

    private func makePanel() -> HUDPanel {
        let p = HUDPanel(contentRect: NSRect(x: 0, y: 0, width: 360, height: 80),
                         styleMask: [.nonactivatingPanel, .borderless, .fullSizeContentView],
                         backing: .buffered, defer: true)
        p.identifier = NSUserInterfaceItemIdentifier("hud")
        p.isFloatingPanel = true
        p.level = .statusBar
        p.hidesOnDeactivate = false
        p.becomesKeyOnlyIfNeeded = true
        p.isOpaque = false
        p.backgroundColor = .clear
        p.hasShadow = true
        p.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]
        p.isReleasedWhenClosed = false
        return p
    }

    /// `--show hud`: a representative "opened" HUD for screenshots.
    func showDemo(kind: String) {
        let c = PathDetector.detect("crates/baton-daemon/src/api/read.rs:120", home: NSHomeDirectory())!
        let base = PathBase(path: NSHomeDirectory() + "/GitHub/baton", source: "tmux claude-7f3a", tier: 0)
        let m = PathMatch(path: base.path + "/crates/baton-daemon/src/api/read.rs", isDirectory: false, base: base)
        switch kind {
        case "choose":
            let c2 = PathDetector.detect("lib.rs", home: NSHomeDirectory())!
            let b2 = PathBase(path: NSHomeDirectory() + "/GitHub/search-hub", source: "session claude-19ab", tier: 2)
            show(.choose(c2, [
                PathMatch(path: base.path + "/crates/baton-core/src/lib.rs", isDirectory: false, base: base),
                PathMatch(path: base.path + "/crates/baton-daemon/src/lib.rs", isDirectory: false, base: base),
                PathMatch(path: b2.path + "/src/lib.rs", isDirectory: false, base: b2),
            ]), persistent: true)
        case "nomatch":
            show(.noMatch(PathDetector.detect("missing_file.rs", home: NSHomeDirectory())!), persistent: true)
        case "ask":
            show(.ask(c, m), persistent: true)
        default:
            show(.opened(c, m, .opened("Cursor")), persistent: true)
        }
    }
}

final class HUDPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
}

@MainActor
final class HoverState: ObservableObject {
    @Published var inside = false
}

struct ClipHUDView: View {
    let content: ClipHUD.Content
    @ObservedObject var hover: HoverState
    let close: () -> Void
    @EnvironmentObject var clip: ClipWatcher

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            switch content {
            case .opened(let c, let m, let done):
                header(c, m, status: status(done))
                HStack(spacing: 8) {
                    Spacer()
                    Button("Reveal") { ClipActions.reveal(m.path); close() }
                    Button("Quick Look") { ClipActions.quickLook(m.path); close() }
                }
            case .ask(let c, let m):
                header(c, m, status: "Copied from the terminal")
                HStack(spacing: 8) {
                    Button("Dismiss") { close() }
                    Spacer()
                    Button("Reveal") { ClipActions.reveal(m.path); close() }
                    Button("Quick Look") { ClipActions.quickLook(m.path); close() }
                    Button("Open") { clip.open(m, for: c) }
                        .buttonStyle(.glassProminent)
                }
            case .choose(let c, let ms):
                HStack(spacing: 8) {
                    Image(systemName: "doc.on.doc").foregroundStyle(.secondary)
                    Text("\(Text("\(ms.count) files match").foregroundStyle(.secondary)) \(Text(c.displayName).font(.body.monospaced().weight(.medium)))")
                    Spacer()
                    Button { close() } label: { Image(systemName: "xmark") }
                        .buttonStyle(.borderless)
                }
                VStack(spacing: 2) {
                    ForEach(ms.prefix(8), id: \.path) { m in
                        ChoiceRow(m: m) { clip.open(m, for: c) }
                    }
                }
            case .noMatch(let c):
                HStack(spacing: 8) {
                    Image(systemName: "questionmark.folder").foregroundStyle(.secondary)
                    Text("\(Text("No file matches").foregroundStyle(.secondary)) \(Text(c.displayName).font(.body.monospaced()))")
                }
            }
        }
        .padding(14)
        .frame(width: width, alignment: .leading)
        // cacheDisplay (the snapshot fallback while the screen is locked)
        // can't draw glass; give it the window colour underneath.
        .background(LaunchOptions.isSnapshot ? Color(nsColor: .windowBackgroundColor) : .clear,
                    in: .rect(cornerRadius: 18))
        .glassEffect(.regular, in: .rect(cornerRadius: 18))
        .onHover { hover.inside = $0 }
        .controlSize(.small)
    }

    private var width: CGFloat {
        if case .noMatch = content { return 300 }
        return 380
    }

    private func status(_ d: ClipActions.Done) -> String {
        switch d {
        case .opened(let app): "Opened in \(app)"
        case .revealed: "Shown in Finder"
        case .quickLook: "Quick Look"
        }
    }

    @ViewBuilder
    private func header(_ c: PathCandidate, _ m: PathMatch, status: String) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(nsImage: NSWorkspace.shared.icon(forFile: m.path))
                .resizable().frame(width: 34, height: 34)
            VStack(alignment: .leading, spacing: 2) {
                Text(c.line.map { "\((m.path as NSString).lastPathComponent):\($0)" } ?? (m.path as NSString).lastPathComponent)
                    .font(.headline.monospaced()).lineLimit(1).truncationMode(.middle)
                Text(where_(m)).font(.caption).foregroundStyle(.secondary)
                    .lineLimit(2).truncationMode(.head)
                Text(status).font(.caption2).foregroundStyle(.tertiary)
            }
        }
    }

    /// "crates/baton-daemon/src/api · in ~/GitHub/baton (tmux claude-7f3a)"
    private func where_(_ m: PathMatch) -> String {
        let dir = (m.path as NSString).deletingLastPathComponent
        guard let b = m.base else { return tilde(dir) }
        var rel = dir.hasPrefix(b.path) ? String(dir.dropFirst(b.path.count)) : dir
        if rel.hasPrefix("/") { rel.removeFirst() }
        let head = rel.isEmpty ? "" : rel + " · "
        return "\(head)in \(tilde(b.path)) (\(b.source))"
    }
}

private func tilde(_ p: String) -> String {
    let h = NSHomeDirectory()
    return p.hasPrefix(h) ? "~" + p.dropFirst(h.count) : p
}

private struct ChoiceRow: View {
    let m: PathMatch
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(nsImage: NSWorkspace.shared.icon(forFile: m.path)).resizable().frame(width: 18, height: 18)
                VStack(alignment: .leading, spacing: 1) {
                    Text(relative).font(.callout.monospaced()).lineLimit(1).truncationMode(.head)
                    if let b = m.base {
                        Text("\(tilde(b.path)) · \(b.source)").font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                Spacer(minLength: 0)
            }
            .padding(.vertical, 5).padding(.horizontal, 8)
            .contentShape(Rectangle())
            .background(hovering ? Color.primary.opacity(0.08) : .clear, in: .rect(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }

    private var relative: String {
        guard let b = m.base, m.path.hasPrefix(b.path + "/") else { return tilde(m.path) }
        return String(m.path.dropFirst(b.path.count + 1))
    }
}
