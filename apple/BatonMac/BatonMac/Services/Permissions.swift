import AppKit
import ApplicationServices
import CoreServices
import UserNotifications

/// `.pending` is neutral (optional or not asked yet); `.attention` is
/// reserved for required things.
enum Level: Equatable {
    case ok, pending, attention, bad, checking
}

struct CheckStatus: Equatable {
    var level: Level
    var detail: String
    static let checking = CheckStatus(level: .checking, detail: "Checking…")
}

enum SettingsLink {
    static let automation = "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation"
    static let files = "x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders"
    static let accessibility = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
    static let notifications = "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
    static let loginItems = "x-apple.systempreferences:com.apple.LoginItems-Settings.extension"

    static func open(_ s: String) {
        if let u = URL(string: s) { NSWorkspace.shared.open(u) }
    }
}

/// Live status of the macOS privacy permissions Baton needs. Every check that
/// could block (AppleEvents consent, a TCC-guarded directory listing) runs off
/// the main thread with a deadline — a hang means "waiting for approval".
@MainActor
final class Permissions: ObservableObject {
    @Published var automation = CheckStatus.checking
    @Published var documents = CheckStatus.checking
    @Published var accessibility = CheckStatus.checking
    @Published var notifications = CheckStatus.checking

    /// Documents check is only run on demand (onboarding open / Recheck): on
    /// first run it shows the system prompt.
    private var documentsProbeInFlight = false

    /// Only Terminal access is required; the rest are optional extras.
    var requiredMissing: Bool { automation.level != .ok }

    func refreshAll(includeDocuments: Bool) {
        refreshAutomation(ask: false)
        refreshAccessibility()
        Task { await refreshNotifications() }
        if includeDocuments { refreshDocuments() }
    }

    // MARK: Automation → Terminal

    nonisolated static let terminalBundleID = "com.apple.Terminal"

    func refreshAutomation(ask: Bool) {
        if ask { automation = CheckStatus(level: .checking, detail: "Waiting for your answer…") }
        Task.detached {
            let status = Self.automationStatus(ask: ask)
            await MainActor.run { self.automation = status }
        }
    }

    nonisolated static func automationStatus(ask: Bool) -> CheckStatus {
        if ask, NSRunningApplication.runningApplications(withBundleIdentifier: terminalBundleID).isEmpty {
            // Consent can only be asked while the target runs.
            if let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: terminalBundleID) {
                let cfg = NSWorkspace.OpenConfiguration()
                cfg.activates = false
                NSWorkspace.shared.openApplication(at: url, configuration: cfg)
                Thread.sleep(forTimeInterval: 1.5)
            }
        }
        let target = NSAppleEventDescriptor(bundleIdentifier: terminalBundleID)
        guard let desc = target.aeDesc else {
            return CheckStatus(level: .bad, detail: "could not address Terminal")
        }
        let err = AEDeterminePermissionToAutomateTarget(desc, typeWildCard, typeWildCard, ask)
        switch err {
        case noErr:
            return CheckStatus(level: .ok, detail: "Allowed")
        case OSStatus(errAEEventNotPermitted):
            return CheckStatus(level: .bad, detail: "Turned off in System Settings")
        case OSStatus(errAEEventWouldRequireUserConsent):
            return CheckStatus(level: .attention, detail: "Not asked yet")
        case OSStatus(procNotFound):
            return CheckStatus(level: .attention, detail: "Unknown until Terminal is open")
        default:
            return CheckStatus(level: .attention, detail: "Unknown (OSStatus \(err))")
        }
    }

    // MARK: Files & Folders → Documents

    func refreshDocuments() {
        guard !documentsProbeInFlight else { return }
        documentsProbeInFlight = true
        documents = CheckStatus(level: .checking, detail: "Checking…")
        let dir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Documents").path
        // The listing runs on its own thread: if TCC blocks it behind a
        // prompt, the thread waits, the UI doesn't.
        let box = ProbeBox()
        Thread.detachNewThread {
            let result: CheckStatus
            do {
                let n = try FileManager.default.contentsOfDirectory(atPath: dir).count
                result = CheckStatus(level: .ok, detail: "Allowed (\(n) items)")
            } catch let e as NSError {
                let posix = (e.userInfo[NSUnderlyingErrorKey] as? NSError)?.code
                if posix == Int(EPERM) || e.code == NSFileReadNoPermissionError {
                    result = CheckStatus(level: .pending, detail: "Off")
                } else if e.code == NSFileReadNoSuchFileError {
                    result = CheckStatus(level: .ok, detail: "No Documents folder")
                } else {
                    result = CheckStatus(level: .pending, detail: e.localizedDescription)
                }
            }
            box.finish(result)
            Task { @MainActor in
                self.documents = result
                self.documentsProbeInFlight = false
            }
        }
        Task {
            try? await Task.sleep(for: .seconds(3))
            if !box.done {
                self.documents = CheckStatus(level: .pending, detail: "Waiting for approval")
            }
        }
    }

    // MARK: Accessibility

    func refreshAccessibility() {
        accessibility = AXIsProcessTrusted()
            ? CheckStatus(level: .ok, detail: "Allowed")
            : CheckStatus(level: .pending, detail: "Off")
    }

    func requestAccessibility() {
        let key = kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String
        _ = AXIsProcessTrustedWithOptions([key: true] as CFDictionary)
        refreshAccessibility()
    }

    // MARK: Notifications

    func refreshNotifications() async {
        let s = await UNUserNotificationCenter.current().notificationSettings()
        notifications = switch s.authorizationStatus {
        case .authorized, .provisional: CheckStatus(level: .ok, detail: "On")
        case .denied: CheckStatus(level: .pending, detail: "Off in System Settings")
        case .notDetermined: CheckStatus(level: .pending, detail: "Off")
        default: CheckStatus(level: .pending, detail: "Status \(s.authorizationStatus.rawValue)")
        }
    }

    func requestNotifications() {
        Task {
            _ = try? await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge])
            await refreshNotifications()
        }
    }
}

private final class ProbeBox: @unchecked Sendable {
    private let lock = NSLock()
    private var _done = false
    var done: Bool { lock.lock(); defer { lock.unlock() }; return _done }
    func finish(_: CheckStatus) { lock.lock(); _done = true; lock.unlock() }
}
