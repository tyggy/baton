import SwiftUI

struct StatusDot: View {
    let level: Level
    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 10, height: 10)
            .overlay(Circle().stroke(.black.opacity(0.15), lineWidth: 0.5))
            .accessibilityLabel(label)
    }
    private var color: Color {
        switch level {
        case .ok: .green
        case .attention: .orange
        case .bad: .red
        case .checking: .gray.opacity(0.5)
        }
    }
    private var label: String {
        switch level {
        case .ok: "OK"
        case .attention: "Needs attention"
        case .bad: "Problem"
        case .checking: "Checking"
        }
    }
}

/// One onboarding requirement: status dot, name, plain-language reason,
/// live detail, and actions.
struct RequirementRow<Actions: View>: View {
    let title: String
    let reason: String
    let status: CheckStatus
    @ViewBuilder var actions: Actions

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            StatusDot(level: status.level).padding(.top, 5)
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.headline)
                Text(reason).font(.callout).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Text(status.detail).font(.caption.monospaced())
                    .foregroundStyle(status.level == .ok ? Color.secondary : Color.primary)
                    .textSelection(.enabled)
            }
            Spacer(minLength: 8)
            HStack(spacing: 6) { actions }.controlSize(.small)
        }
        .padding(.vertical, 6)
    }
}
