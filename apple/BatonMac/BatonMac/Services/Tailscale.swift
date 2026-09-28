import Foundation

/// Read-only Tailscale status: never changes the user's config.
struct TailscaleInfo: Equatable {
    var cliPath: String?
    var backendState: String?
    var certDomains: [String] = []
    var dnsName: String?          // without trailing dot
    var ipv4: String?
    var servePorts: [String] = [] // local proxy targets from `serve status`
    var error: String?

    var installed: Bool { cliPath != nil }
    var running: Bool { backendState == "Running" }
    var httpsEnabled: Bool { !certDomains.isEmpty }

    func serves(port: String) -> Bool {
        servePorts.contains { $0.hasSuffix(":\(port)") || $0.hasSuffix(":\(port)/") }
    }

    static let candidates = [
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ]

    static func load() -> TailscaleInfo {
        var info = TailscaleInfo()
        guard let cli = candidates.first(where: { FileManager.default.isExecutableFile(atPath: $0) }) else {
            return info
        }
        info.cliPath = cli
        let st = Shell.run(cli, ["status", "--json"], timeout: 8)
        guard st.status == 0,
              let obj = try? JSONSerialization.jsonObject(with: Data(st.stdout.utf8)) as? [String: Any]
        else {
            info.error = st.timedOut ? "tailscale status timed out" : (st.stderr.isEmpty ? "tailscale status failed" : st.stderr)
            return info
        }
        info.backendState = obj["BackendState"] as? String
        info.certDomains = obj["CertDomains"] as? [String] ?? []
        if let me = obj["Self"] as? [String: Any] {
            if let dns = me["DNSName"] as? String, !dns.isEmpty {
                info.dnsName = dns.hasSuffix(".") ? String(dns.dropLast()) : dns
            }
            info.ipv4 = (me["TailscaleIPs"] as? [String])?.first { !$0.contains(":") }
        }
        let sv = Shell.run(cli, ["serve", "status", "--json"], timeout: 8)
        if sv.status == 0, let data = sv.stdout.data(using: .utf8),
           let serve = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            info.servePorts = Self.proxyTargets(serve)
        }
        return info
    }

    /// Collect every `Proxy` value (e.g. "http://127.0.0.1:19877") in the
    /// serve config, wherever it sits (Web → host → Handlers → path).
    static func proxyTargets(_ any: Any) -> [String] {
        var out: [String] = []
        if let d = any as? [String: Any] {
            for (k, v) in d {
                if k == "Proxy", let s = v as? String { out.append(s) } else { out += proxyTargets(v) }
            }
        } else if let a = any as? [Any] {
            for v in a { out += proxyTargets(v) }
        }
        return out
    }
}

@MainActor
final class TailscaleModel: ObservableObject {
    @Published var info: TailscaleInfo?
    @Published var loading = false

    func refresh() {
        guard !loading else { return }
        loading = true
        Task.detached {
            let i = TailscaleInfo.load()
            await MainActor.run {
                self.info = i
                self.loading = false
            }
        }
    }

    var summary: String {
        guard let i = info else { return "Tailscale: checking…" }
        if !i.installed { return "Tailscale: not installed" }
        if let e = i.error { return "Tailscale: \(e)" }
        if !i.running { return "Tailscale: \(i.backendState ?? "not running")" }
        return "Tailscale: \(i.dnsName ?? "connected")\(i.httpsEnabled ? "" : " (HTTPS off)")"
    }
}
