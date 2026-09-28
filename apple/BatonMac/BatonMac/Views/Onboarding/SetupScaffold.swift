import AppKit
import SwiftUI

/// The shared layout of every setup step, after Apple's own welcome sheets:
/// a large symbol, a title, one line of text, and a single glass-prominent
/// button at the bottom with plain links under it.
struct SetupPage<Content: View, Links: View>: View {
    var symbol: String?
    var symbolColor: Color = .accentColor
    var done = false
    let title: String
    let message: String
    /// nil = no primary button (e.g. while waiting for the phone).
    let primaryTitle: String?
    var busy = false
    var primaryEnabled = true
    let primary: () -> Void
    @ViewBuilder var content: Content
    @ViewBuilder var links: Links

    var body: some View {
        VStack(spacing: 0) {
            Spacer(minLength: 28)
            if let symbol {
                StepSymbol(symbol: symbol, color: symbolColor, done: done)
                    .padding(.bottom, 20)
            }
            Text(title)
                .font(.system(size: 28, weight: .semibold))
                .foregroundStyle(.primary)
                // Full-strength label colour: no vibrancy from the window material.
                .environment(\.backgroundMaterial, nil)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            Text(message)
                .font(.title3)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, 8)
            content
                .padding(.top, 20)
            Spacer(minLength: 20)
            if let primaryTitle {
                Button(action: primary) {
                    HStack(spacing: 8) {
                        if busy { ProgressView().controlSize(.small) }
                        Text(primaryTitle)
                    }
                    .frame(minWidth: 220)
                }
                .buttonStyle(.glassProminent)
                .controlSize(.extraLarge)
                .keyboardShortcut(.defaultAction)
                .disabled(busy || !primaryEnabled)
                .transition(.scale(scale: 0.9).combined(with: .opacity))
            }
            VStack(spacing: 8) { links }
                .buttonStyle(.link)
                .font(.callout)
                .padding(.top, 14)
                .frame(minHeight: 22)
        }
        .padding(.horizontal, 48)
        .frame(maxWidth: .infinity)
    }
}

extension SetupPage where Content == EmptyView {
    init(symbol: String?, symbolColor: Color = .accentColor, done: Bool = false, title: String, message: String,
         primaryTitle: String?, busy: Bool = false, primaryEnabled: Bool = true, primary: @escaping () -> Void,
         @ViewBuilder links: () -> Links) {
        self.init(symbol: symbol, symbolColor: symbolColor, done: done, title: title, message: message,
                  primaryTitle: primaryTitle, busy: busy, primaryEnabled: primaryEnabled, primary: primary,
                  content: { EmptyView() }, links: links)
    }
}

/// Large hierarchical symbol that swaps to a green check with a bounce
/// when its step completes (the replace/bounce effects honour Reduce Motion).
struct StepSymbol: View {
    let symbol: String
    var color: Color = .accentColor
    var done: Bool
    var size: CGFloat = 64

    var body: some View {
        Image(systemName: done ? "checkmark.circle.fill" : symbol)
            .font(.system(size: size, weight: .regular))
            .symbolRenderingMode(.hierarchical)
            .foregroundStyle(done ? Color.green : color)
            .contentTransition(.symbolEffect(.replace))
            .symbolEffect(.bounce, value: done)
            .frame(height: size * 1.25)
            .accessibilityHidden(true)
    }
}

/// "Show details" link revealing technical text — the only monospace text
/// in the flow.
struct DetailsDisclosure: View {
    let text: String
    @State private var shown = false

    var body: some View {
        VStack(spacing: 8) {
            Button(shown ? "Hide Details" : "Show Details") {
                withAnimation(.snappy) { shown.toggle() }
            }
            if shown {
                ScrollView {
                    Text(text)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxWidth: 420, maxHeight: 90)
                .padding(10)
                .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .transition(.opacity)
            }
        }
    }
}

/// Reach the hosting NSWindow (to hide the close button on the permission
/// pre-alert).
struct WindowReader: NSViewRepresentable {
    let onWindow: (NSWindow) -> Void
    func makeNSView(context: Context) -> NSView {
        let v = NSView()
        DispatchQueue.main.async { if let w = v.window { onWindow(w) } }
        return v
    }
    func updateNSView(_ v: NSView, context: Context) {
        DispatchQueue.main.async { if let w = v.window { onWindow(w) } }
    }
}
