import AppKit
import SwiftUI

/// "Pair iPhone…" from the menu: the pairing step on its own.
struct PairingView: View {
    @EnvironmentObject var tailscale: TailscaleModel
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        PairPage(pinned: LaunchOptions.isSnapshot, onDone: {}, onNext: { dismissWindow(id: "pairing") }, laterTitle: "Close")
            .padding(.bottom, 28)
            .frame(width: 520, height: 560)
    }
}
