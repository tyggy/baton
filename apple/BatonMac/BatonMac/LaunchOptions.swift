import AppKit

/// Launch arguments for scripted checks and screenshots:
///   --daemon register|unregister
///   --show onboarding|pairing|settings
///   --step N             pin the setup flow to step N (no side effects, no auto-advance)
///   --appearance light|dark
///   --snapshot <png>     render the shown window to a file after 6 s (token replaced)
enum LaunchOptions {
    static let args = ProcessInfo.processInfo.arguments

    static func value(_ flag: String) -> String? {
        guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
        return args[i + 1]
    }

    static let show = value("--show")
    static let daemonAction = value("--daemon")
    static let snapshot = value("--snapshot")
    static let appearance = value("--appearance")
    static let pinnedStep: SetupStep? = value("--step").flatMap(Int.init).flatMap(SetupStep.init(rawValue:))
    static var isSnapshot: Bool { snapshot != nil }

    static var onboardingComplete: Bool {
        get { UserDefaults.standard.bool(forKey: "onboardingComplete") }
        set { UserDefaults.standard.set(newValue, forKey: "onboardingComplete") }
    }

    static var openOnboardingAtLaunch: Bool {
        show == "onboarding" || (show == nil && !onboardingComplete)
    }
}
