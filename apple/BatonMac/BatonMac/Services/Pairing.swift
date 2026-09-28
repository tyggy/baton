import AppKit
import CoreImage
import CoreImage.CIFilterBuiltins

/// Pairing payload for the iPhone app (the iOS side comes later):
/// `{"v":1,"url":"https://<magicdns>","fallback":"http://<ts ipv4>:19877","token":"…"}`
enum Pairing {
    /// `BATON_BEARER` from `~/.claude-sessions/config` (same parsing as the daemon).
    static func loadToken() -> String? {
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

    static func payload(ts: TailscaleInfo?, token: String) -> String? {
        guard let dns = ts?.dnsName else { return nil }
        var obj: [(String, Any)] = [("v", 1), ("url", "https://\(dns)")]
        if let ip = ts?.ipv4 { obj.append(("fallback", "http://\(ip):\(fallbackPort)")) }
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
