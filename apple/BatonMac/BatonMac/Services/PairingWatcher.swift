import Foundation

/// Watches for the first iPhone to connect after the pairing code is shown:
/// the iOS app registers itself in `~/.config/baton/devices.json` on launch,
/// and the request logs record non-local clients (tailnet / LAN peers).
@MainActor
final class PairingWatcher: ObservableObject {
    @Published private(set) var connected = false
    private var task: Task<Void, Never>?

    func start() {
        guard task == nil, !connected else { return }
        let since = Date()
        task = Task { [weak self] in
            while !Task.isCancelled {
                let saw = await Task.detached(priority: .utility) { PairingWatcher.sawPhone(since: since) }.value
                if saw {
                    self?.connected = true
                    return
                }
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    nonisolated static func sawPhone(since: Date) -> Bool {
        let home = FileManager.default.homeDirectoryForCurrentUser
        let devices = home.appendingPathComponent(".config/baton/devices.json")
        if let m = (try? FileManager.default.attributesOfItem(atPath: devices.path))?[.modificationDate] as? Date,
           m > since {
            return true
        }
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd'T'HH:mm:ss"
        let stamp = f.string(from: since)
        for name in ["Library/Logs/Baton/cs-server.requests.jsonl", "Library/Logs/cs-server.requests.jsonl"] {
            for line in tail(home.appendingPathComponent(name), bytes: 64 * 1024) {
                guard let obj = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any],
                      let ts = obj["ts"] as? String, ts >= stamp,
                      let path = obj["path"] as? String, path != "/api/health",
                      let st = obj["st"] as? Int, (200..<400).contains(st),
                      let client = obj["client"] as? String
                else { continue }
                if client != "-" && !client.hasPrefix("127.") && !client.hasPrefix("[::1]") && !client.hasPrefix("::1") {
                    return true
                }
            }
        }
        return false
    }

    nonisolated private static func tail(_ url: URL, bytes: Int) -> [String] {
        guard let h = try? FileHandle(forReadingFrom: url) else { return [] }
        defer { try? h.close() }
        let end = (try? h.seekToEnd()) ?? 0
        try? h.seek(toOffset: end > UInt64(bytes) ? end - UInt64(bytes) : 0)
        let data = (try? h.readToEnd()) ?? Data()
        return String(decoding: data, as: UTF8.self).split(separator: "\n").dropFirst(end > UInt64(bytes) ? 1 : 0).map(String.init)
    }
}
