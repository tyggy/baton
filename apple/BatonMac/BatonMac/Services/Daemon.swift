import Foundation
import ServiceManagement

/// Hosts `baton serve` as a LaunchAgent registered through SMAppService, so
/// the daemon runs under this app's identity (TCC prompts are attributed to
/// Baton, not to an anonymous launchd job — see docs/incidents.md INC-002).
@MainActor
final class DaemonController: ObservableObject {
    enum Registration: Equatable {
        case notRegistered, enabled, requiresApproval, notFound, unknown(String)
        var label: String {
            switch self {
            case .notRegistered: "not registered"
            case .enabled: "enabled"
            case .requiresApproval: "needs approval in Login Items"
            case .notFound: "LaunchAgent plist missing from app bundle"
            case .unknown(let s): s
            }
        }
    }

    struct Health: Equatable {
        var daemon: String?
        var apiVersion: Int?
        var release: String?
        var pid: Int?
        var status: String
    }

    enum Liveness: Equatable {
        case unknown, running(Health), stopped, error(String)
    }

    @Published private(set) var registration: Registration = .notRegistered
    @Published private(set) var liveness: Liveness = .unknown
    @Published var lastError: String?

    let label: String
    private var timer: Timer?

    init() {
        label = Bundle.main.object(forInfoDictionaryKey: "BatonDaemonLabel") as? String
            ?? "com.torwood.baton.daemon"
        refreshRegistration()
    }

    private var service: SMAppService { SMAppService.agent(plistName: "\(label).plist") }

    // MARK: config

    static let defaultListen = "127.0.0.1:19880"
    static let defaultUpstream = "http://127.0.0.1:19877"

    var listen: String {
        get { UserDefaults.standard.string(forKey: "daemon.listen") ?? Self.defaultListen }
        set { UserDefaults.standard.set(newValue, forKey: "daemon.listen") }
    }
    var upstream: String {
        get { UserDefaults.standard.string(forKey: "daemon.upstream") ?? Self.defaultUpstream }
        set { UserDefaults.standard.set(newValue, forKey: "daemon.upstream") }
    }

    /// Port the daemon listens on (for pairing / tailscale serve hints).
    var listenPort: String { listen.split(separator: ":").last.map(String.init) ?? "19880" }

    static var configURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/Baton/daemon.conf")
    }
    static var logsURL: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/Baton")
    }

    /// The LaunchAgent has fixed arguments (`serve --config <this file>`);
    /// listen/upstream come from here. Written before every (re)start.
    func writeConfig() {
        let text = """
        # Written by Baton.app — edit in the app (Onboarding → Daemon).
        listen=\(listen)
        upstream=\(upstream)

        """
        do {
            try FileManager.default.createDirectory(
                at: Self.configURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            try text.write(to: Self.configURL, atomically: true, encoding: .utf8)
        } catch {
            lastError = "config: \(error.localizedDescription)"
        }
    }

    // MARK: lifecycle

    func refreshRegistration() {
        registration = switch service.status {
        case .notRegistered: .notRegistered
        case .enabled: .enabled
        case .requiresApproval: .requiresApproval
        case .notFound: .notFound
        @unknown default: .unknown("status \(service.status.rawValue)")
        }
    }

    func register() {
        writeConfig()
        do {
            try service.register()
            lastError = nil
        } catch {
            lastError = "register: \(error.localizedDescription)"
        }
        refreshRegistration()
        Task { await pollHealth() }
    }

    func unregister() {
        Task {
            do {
                try await service.unregister()
                lastError = nil
            } catch {
                lastError = "unregister: \(error.localizedDescription)"
            }
            refreshRegistration()
            await pollHealth()
        }
    }

    /// `launchctl kickstart -k` restarts the job in place (keeps the registration).
    func restart() {
        writeConfig()
        let target = "gui/\(getuid())/\(label)"
        Task.detached {
            let r = Shell.run("/bin/launchctl", ["kickstart", "-k", target], timeout: 10)
            await MainActor.run {
                if r.status != 0 {
                    self.lastError = "restart: \(r.stderr.isEmpty ? "exit \(r.status)" : r.stderr)"
                }
            }
            try? await Task.sleep(for: .seconds(1))
            await self.pollHealth()
        }
    }

    enum StartOutcome: Equatable {
        case running, needsApproval, failed(String)
    }

    /// Register (if needed) and wait until `/api/health` answers.
    func ensureRunning() async -> StartOutcome {
        writeConfig()
        refreshRegistration()
        if registration != .enabled {
            do {
                try service.register()
            } catch {
                refreshRegistration()
                if registration == .requiresApproval { return .needsApproval }
                return .failed("SMAppService.agent(\(label).plist).register(): \(error.localizedDescription)\nstatus: \(registration.label)")
            }
            refreshRegistration()
        }
        if registration == .requiresApproval { return .needsApproval }
        for _ in 0..<20 {
            await pollHealth()
            if case .running = liveness { return .running }
            try? await Task.sleep(for: .milliseconds(750))
        }
        return .failed("""
            \(statusLine)
            LaunchAgent \(label): \(registration.label)
            listen \(listen), upstream \(upstream)
            stderr: /tmp/baton-daemon.err.log
            """)
    }

    var technicalSummary: String {
        """
        \(statusLine)
        LaunchAgent \(label): \(registration.label)
        listen \(listen) · upstream \(upstream)
        config \(Self.configURL.path)
        """
    }

    func openLoginItems() {
        SMAppService.openSystemSettingsLoginItems()
    }

    // MARK: health

    func startPolling() {
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 10, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.refreshRegistration()
                await self?.pollHealth()
            }
        }
        Task { await pollHealth() }
    }

    func pollHealth() async {
        guard let url = URL(string: "http://\(listen)/api/health") else {
            liveness = .error("bad listen address")
            return
        }
        var req = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 3)
        req.httpMethod = "GET"
        do {
            let (data, resp) = try await URLSession.shared.data(for: req)
            guard (resp as? HTTPURLResponse)?.statusCode == 200,
                  let obj = try JSONSerialization.jsonObject(with: data) as? [String: Any]
            else {
                liveness = .error("unexpected /api/health response")
                return
            }
            let h = Health(
                daemon: obj["daemon"] as? String,
                apiVersion: obj["api_version"] as? Int,
                release: obj["release"] as? String,
                pid: obj["pid"] as? Int,
                status: obj["status"] as? String ?? "?")
            if h.daemon == nil {
                // Something answers, but it isn't the Rust daemon (e.g. Python).
                liveness = .error("\(listen) is not the baton daemon")
            } else {
                liveness = .running(h)
            }
        } catch let e as URLError where e.code == .cannotConnectToHost || e.code == .timedOut {
            liveness = .stopped
        } catch {
            liveness = .error(error.localizedDescription)
        }
    }

    var statusLine: String {
        switch liveness {
        case .unknown: "Daemon: checking…"
        case .running(let h): "Daemon: running v\(h.daemon ?? "?") on \(listen)"
        case .stopped: registration == .requiresApproval
            ? "Daemon: stopped (needs approval)" : "Daemon: stopped"
        case .error(let e): "Daemon: error — \(e)"
        }
    }
}
