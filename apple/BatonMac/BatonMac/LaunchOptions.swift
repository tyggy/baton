import AppKit

/// Launch arguments for scripted checks and screenshots:
///   --daemon register|unregister
///   --show onboarding|pairing|settings
///   --step N             pin the setup flow to step N (no side effects, no auto-advance)
///   --appearance light|dark
///   --snapshot <png>     render the shown window to a file after 6 s (token replaced)
///   --show hud --hud opened|ask|choose|nomatch   the copy-to-open HUD (demo content)
///   --settings-tab status|clip|advanced
///   --allow-inactive     snapshot even if the window never became key (locked screen)
///   --clip-resolve <text> print what a copied <text> resolves to, then quit (no pasteboard)
///   --clip-base <dir>    extra base directory for partial paths (repeatable)
///   --clip-debug         log every clipboard step to stderr
enum LaunchOptions {
    static let args = ProcessInfo.processInfo.arguments

    static func value(_ flag: String) -> String? {
        guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
        return args[i + 1]
    }

    /// Every value of a repeatable flag.
    static func values(_ flag: String) -> [String] {
        args.indices.compactMap { i in args[i] == flag && i + 1 < args.count ? args[i + 1] : nil }
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
        show == "onboarding" || (show == nil && !onboardingComplete && value("--clip-resolve") == nil)
    }
}
