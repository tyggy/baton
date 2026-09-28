import AppKit
import CoreImage
import CoreImage.CIFilterBuiltins

/// Pairing payload for the iPhone app (the iOS side comes later):
/// `{"v":1,"url":"https://<magicdns>","fallback":"http://<ts ipv4>:19877","token":"…"}`
enum Pairing {
    /// `BATON_BEARER` from `~/.claude-sessions/config` (same parsing as the daemon).
    static func loadToken() -> String? {
        // Screenshots never carry the real token (the QR encodes it).
        if LaunchOptions.isSnapshot { return "snapshot-placeholder-token" }
        if let env = ProcessInfo.processInfo.environment["BATON_BEARER"], !env.isEmpty { return env }
        let url = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".claude-sessions/config")
        guard let text = try? String(contentsOf: url, encoding: .utf8) else { return nil }
        var token: String?
        for raw in text.split(whereSeparator: \.isNewline) {
            var l = raw.trimmingCharacters(in: .whitespaces)
            if l.hasPrefix("export ") { l = String(l.dropFirst(7)).trimmingCharacters(in: .whitespaces) }
            guard l.hasPrefix("BATON_BEARER=") else { continue }
            token = String(l.dropFirst("BATON_BEARER=".count))
                .trimmingCharacters(in: .whitespaces)
                .trimmingCharacters(in: CharacterSet(charactersIn: "\"'"))
        }
        return (token?.isEmpty ?? true) ? nil : token
    }

    static let fallbackPort = "19877"

    /// Primary URL is the tailnet HTTPS name; the fallback is plain HTTP to
    /// the tailnet IP, or to the LAN address when Tailscale isn't set up
    /// (home Wi-Fi only).
    static func payload(ts: TailscaleInfo?, token: String) -> String? {
        let fallbackHost = ts?.ipv4 ?? lanIPv4()
        let url: String
        if let dns = ts?.dnsName {
            url = "https://\(dns)"
        } else if let host = fallbackHost {
            url = "http://\(host):\(fallbackPort)"
        } else {
            return nil
        }
        var obj: [(String, Any)] = [("v", 1), ("url", url)]
        if let host = fallbackHost { obj.append(("fallback", "http://\(host):\(fallbackPort)")) }
        obj.append(("token", token))
        // Stable key order (JSONSerialization doesn't keep insertion order).
        let parts = obj.map { k, v -> String in
            let val: String
            if let s = v as? String {
                let enc = (try? JSONSerialization.data(withJSONObject: s, options: .fragmentsAllowed)) ?? Data()
                val = String(decoding: enc, as: UTF8.self)
            } else {
                val = "\(v)"
            }
            return "\"\(k)\":\(val)"
        }
        return "{" + parts.joined(separator: ",") + "}"
    }

    static func lanIPv4() -> String? {
        var ifap: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&ifap) == 0, let first = ifap else { return nil }
        defer { freeifaddrs(ifap) }
        var found: String?
        for p in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let ifa = p.pointee
            guard let addr = ifa.ifa_addr, addr.pointee.sa_family == UInt8(AF_INET) else { continue }
            let name = String(cString: ifa.ifa_name)
            guard name.hasPrefix("en") else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            if getnameinfo(addr, socklen_t(addr.pointee.sa_len), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 {
                let ip = String(cString: host)
                if found == nil || name == "en0" { found = ip }
            }
        }
        return found
    }

    static func mask(_ token: String) -> String {
        guard token.count > 8 else { return String(repeating: "•", count: token.count) }
        return String(token.prefix(4)) + String(repeating: "•", count: 12) + String(token.suffix(2))
    }

    static func qrImage(_ text: String, scale: CGFloat = 10) -> NSImage? {
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(text.utf8)
        f.correctionLevel = "M"
        guard let out = f.outputImage?.transformed(by: CGAffineTransform(scaleX: scale, y: scale)) else { return nil }
        let rep = NSCIImageRep(ciImage: out)
        let img = NSImage(size: rep.size)
        img.addRepresentation(rep)
        return img
    }
}
