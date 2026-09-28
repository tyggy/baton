import SwiftUI

/// Status as an SF Symbol: filled check when done, a neutral circle when
/// pending or optional; orange/red only for required things that are wrong.
struct StatusSymbol: View {
    let level: Level
    var body: some View {
        Group {
            switch level {
            case .ok: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
            case .pending: Image(systemName: "circle").foregroundStyle(.secondary)
            case .attention: Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange)
            case .bad: Image(systemName: "xmark.circle.fill").foregroundStyle(.red)
            case .checking: ProgressView().controlSize(.mini)
            }
        }
        .frame(width: 18)
        .accessibilityLabel(label)
    }
    private var label: String {
        switch level {
        case .ok: "Done"
        case .pending: "Off"
        case .attention: "Needs attention"
        case .bad: "Problem"
        case .checking: "Checking"
        }
    }
}

/// One settings row: status symbol, title with a short reason, status text
/// and actions on the trailing side.
struct StatusRow<Actions: View>: View {
    let title: String
    let reason: String
    let status: CheckStatus
    @ViewBuilder var actions: Actions

    var body: some View {
        LabeledContent {
            HStack(spacing: 8) {
                Text(status.detail).foregroundStyle(.secondary).lineLimit(2)
                actions
            }
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                StatusSymbol(level: status.level)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                    Text(reason).font(.caption).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }
}
