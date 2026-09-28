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

    /// Addresses a phone should use. The Tailscale name is known as soon as
    /// the Mac is on a tailnet — before remote access is turned on — so a
    /// phone paired first still learns it; `lan` covers home Wi-Fi.
    struct Addresses: Equatable {
        var url: String
        var fallback: String?
        var lan: String?
    }

    static func addresses(ts: TailscaleInfo?) -> Addresses? {
        let lan = lanIPv4().map { "http://\($0):\(fallbackPort)" }
        let tsHTTP = ts?.ipv4.map { "http://\($0):\(fallbackPort)" }
        if let dns = ts?.dnsName {
            return Addresses(url: "https://\(dns)", fallback: tsHTTP ?? lan, lan: lan)
        }
        guard let first = tsHTTP ?? lan else { return nil }
        return Addresses(url: first, fallback: tsHTTP == nil ? nil : lan, lan: lan)
    }

    /// `{"v":1,"url":…,"fallback":…,"lan":…,"token":…}` — keys in this order.
    static func payload(ts: TailscaleInfo?, token: String) -> String? {
        guard let a = addresses(ts: ts) else { return nil }
        var pairs: [(String, Any)] = [("v", 1), ("url", a.url)]
        if let f = a.fallback { pairs.append(("fallback", f)) }
        if let l = a.lan { pairs.append(("lan", l)) }
        pairs.append(("token", token))
        return encode(pairs)
    }

    /// `pairing.json` for the daemon's `GET /api/pairing` (no token): phones
    /// re-read it after pairing, so the order of pairing and remote access
    /// doesn't matter (docs/api.md).
    static func publish(ts: TailscaleInfo?) {
        guard !LaunchOptions.isSnapshot, let a = addresses(ts: ts) else { return }
        var pairs: [(String, Any)] = [("v", 1), ("url", a.url)]
        if let f = a.fallback { pairs.append(("fallback", f)) }
        if let l = a.lan { pairs.append(("lan", l)) }
        pairs.append(("remote_access", ts.map { $0.servePorts.isEmpty ? false : true } ?? false))
        pairs.append(("updated", ISO8601DateFormatter().string(from: Date())))
        let url = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/Baton/pairing.json")
        let text = encode(pairs) + "\n"
        if (try? String(contentsOf: url, encoding: .utf8))?.replacingOccurrences(
            of: #""updated":"[^"]*""#, with: "", options: .regularExpression)
            == text.replacingOccurrences(of: #""updated":"[^"]*""#, with: "", options: .regularExpression) {
            return // unchanged apart from the timestamp
        }
        try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? text.write(to: url, atomically: true, encoding: .utf8)
    }

    private static func encode(_ pairs: [(String, Any)]) -> String {
        let parts = pairs.map { k, v -> String in
            let val: String
            if let s = v as? String {
                let enc = (try? JSONSerialization.data(withJSONObject: s, options: .fragmentsAllowed)) ?? Data()
                val = String(decoding: enc, as: UTF8.self)
            } else if let b = v as? Bool {
                val = b ? "true" : "false"
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
