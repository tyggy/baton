import AppKit
import SwiftUI

enum SetupStep: Int, CaseIterable {
    case welcome = 1, terminal, pair, remote
    /// Brief closing confirmation (not counted in the progress dots).
    case finished
}

/// First-run flow whose only goal is "your iPhone is connected".
/// Each step advances by itself as soon as its check passes.
struct SetupFlow: View {
    @EnvironmentObject var daemon: DaemonController
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.dismissWindow) private var dismissWindow

    @State private var step: SetupStep = LaunchOptions.pinnedStep ?? .welcome
    @State private var forward = true
    @State private var window: NSWindow?
    private let pinned = LaunchOptions.pinnedStep != nil

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                page(step)
                    .id(step)
                    .transition(transition)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            footer
                .padding(.horizontal, 22)
                .padding(.top, 14)
                .padding(.bottom, 20)
        }
        .frame(width: 520, height: 580)
        .background(WindowReader { w in
            if window !== w { window = w; NSApp.activate() }
            // Permission pre-alert: no way out but Continue (HIG).
            w.standardWindowButton(.closeButton)?.isHidden = (step == .terminal)
        })
    }

    private var transition: AnyTransition {
        if reduceMotion { return .opacity }
        return .asymmetric(
            insertion: .move(edge: forward ? .trailing : .leading).combined(with: .opacity),
            removal: .move(edge: forward ? .leading : .trailing).combined(with: .opacity))
    }

    private func go(_ s: SetupStep) {
        forward = s.rawValue > step.rawValue
        withAnimation(reduceMotion ? .easeInOut(duration: 0.2) : .spring(duration: 0.5, bounce: 0.12)) {
            step = s
        }
        window?.standardWindowButton(.closeButton)?.isHidden = (s == .terminal)
    }

    /// Advance after a short beat so the completed check is seen.
    private func complete(_ from: SetupStep, delay: Double = 0.9) {
        guard !pinned else { return }
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(delay))
            if step == from { next() }
        }
    }

    private func next() {
        guard let n = SetupStep(rawValue: step.rawValue + 1) else { return }
        if n == .finished { LaunchOptions.onboardingComplete = true }
        go(n)
    }

    @ViewBuilder private func page(_ s: SetupStep) -> some View {
        switch s {
        case .welcome: WelcomePage(pinned: pinned, onDone: { complete(.welcome) })
        case .terminal: TerminalPage(pinned: pinned, onDone: { complete(.terminal) }, onSkip: next)
        case .pair: PairPage(pinned: pinned, onDone: { complete(.pair, delay: 1.4) }, onNext: next)
        case .remote: RemotePage(pinned: pinned, onDone: { complete(.remote) }, onLater: next)
        case .finished: FinishedPage(pinned: pinned, onClose: close)
        }
    }

    private func close() {
        LaunchOptions.onboardingComplete = true
        dismissWindow(id: "onboarding")
    }

    private var footer: some View {
        HStack {
            backButton
            Spacer()
            HStack(spacing: 8) {
                ForEach(SetupStep.allCases.filter { $0 != .finished }, id: \.self) { s in
                    Circle()
                        .fill(s == step ? Color.accentColor : Color.secondary.opacity(0.3))
                        .frame(width: 7, height: 7)
                }
            }
            .opacity(step == .finished ? 0 : 1)
            .accessibilityElement()
            .accessibilityLabel("Step \(min(step.rawValue, 4)) of 4")
            Spacer()
            backButton.hidden()
        }
    }

    @ViewBuilder private var backButton: some View {
        // Back is offered after the permission step (never on the pre-alert).
        if step == .pair || step == .remote {
            Button {
                go(SetupStep(rawValue: step.rawValue - 1)!)
            } label: {
                Label("Back", systemImage: "chevron.backward").labelStyle(.titleAndIcon)
            }
            .buttonStyle(.link)
            .font(.callout)
        } else {
            Text("Back").font(.callout).hidden()
        }
    }
}
