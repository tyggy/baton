import AppKit
import SwiftUI

struct PairingView: View {
    @EnvironmentObject var tailscale: TailscaleModel
    @State private var token: String? = Pairing.loadToken()
    @State private var copied = false

    var body: some View {
        VStack(spacing: 16) {
            Text("Pair your iPhone").font(.title.bold())
            Text("In Baton on your iPhone, scan this code. It holds this Mac's address and your access token — treat it like a password.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            content
        }
        .padding(24)
        .frame(minWidth: 400, minHeight: 540)
        .onAppear { token = Pairing.loadToken() }
    }

    @ViewBuilder private var content: some View {
        if let token {
            if let payload = Pairing.payload(ts: tailscale.info, token: token) {
                if let img = Pairing.qrImage(payload) {
                    Image(nsImage: img)
                        .interpolation(.none)
                        .resizable()
                        .frame(width: 260, height: 260)
                        .padding(10)
                        .background(Color.white)
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                }
                Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 4) {
                    GridRow { Text("URL").foregroundStyle(.secondary); Text("https://\(tailscale.info?.dnsName ?? "")") }
                    if let ip = tailscale.info?.ipv4 {
                        GridRow { Text("Fallback").foregroundStyle(.secondary); Text("http://\(ip):\(Pairing.fallbackPort)") }
                    }
                    GridRow { Text("Token").foregroundStyle(.secondary); Text(Pairing.mask(token)) }
                }
                .font(.callout.monospaced())
                Button(copied ? "Copied" : "Copy pairing code") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(payload, forType: .string)
                    copied = true
                }
            } else if tailscale.loading || tailscale.info == nil {
                ProgressView("Reading Tailscale status…")
            } else {
                Label("Tailscale isn't connected, so there's no address to pair with. Fix it in Onboarding first.",
                      systemImage: "exclamationmark.triangle")
            }
        } else {
            Label("No BATON_BEARER in ~/.claude-sessions/config. Set a token there first.",
                  systemImage: "key.slash")
        }
    }
}
