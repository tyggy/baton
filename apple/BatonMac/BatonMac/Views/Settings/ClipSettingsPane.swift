import AppKit
import PathDetect
import SwiftUI

/// Settings → Copy to Open.
struct ClipSettingsPane: View {
    @EnvironmentObject var clip: ClipWatcher
    @State private var testText = ""
    @State private var testResult = ""

    var body: some View {
        Form {
            Section {
                Toggle(isOn: $clip.enabled) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Open paths you copy")
                        Text("Copy a file path from a Claude Code session — even a partial one like src/api/read.rs:120 or read.rs — and Baton finds it in that session's folder and opens it.")
                            .font(.caption).foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                if clip.enabled {
                    Toggle("Paused", isOn: $clip.paused)
                }
                Picker("When a path is copied", selection: $clip.mode) {
                    ForEach(ClipWatcher.Mode.allCases) { Text($0.label).tag($0) }
                }
                Picker("Files with a line number", selection: $clip.lineEditor) {
                    Text("Default app").tag(String?.none)
                    ForEach(ClipActions.installedEditors) { e in Text(e.name).tag(Optional(e.bundleID)) }
                }
                Toggle("Tell me when nothing matches", isOn: $clip.showNoMatch)
            }
            Section("Privacy") {
                StatusRow(title: "Clipboard access",
                          reason: "macOS asks before an app reads what you copied. Choose Always Allow for Baton in Privacy & Security → Paste to stop the prompt.",
                          status: accessStatus) {
                    Button("Settings…") { SettingsLink.open(SettingsLink.paste) }
                }
            }
            Section {
                Toggle("Only when copying in a terminal", isOn: $clip.terminalOnly)
                ForEach(clip.terminalApps.filter(isInstalled), id: \.self) { id in
                    HStack {
                        AppLabel(bundleID: id)
                        Spacer()
                        Button { clip.terminalApps.removeAll { $0 == id } } label: { Image(systemName: "minus.circle") }
                            .buttonStyle(.borderless)
                            .help("Remove")
                    }
                    .disabled(!clip.terminalOnly)
                }
                HStack {
                    Menu("Add App") {
                        ForEach(runningApps, id: \.self) { id in
                            Button { clip.terminalApps.append(id) } label: { Text(appName(id)) }
                        }
                    }
                    .fixedSize()
                    Spacer()
                    Button("Reset to Defaults") { clip.terminalApps = ClipWatcher.defaultTerminals }
                }
                .disabled(!clip.terminalOnly)
            } header: {
                Text("Terminals")
            } footer: {
                let missing = clip.terminalApps.filter { !isInstalled($0) }.count
                Text("Paths are only read from the clipboard when one of these apps is in front\(missing > 0 ? " (plus \(missing) listed but not installed)" : ""). Password-manager copies (marked concealed or transient) are always ignored.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Try it") {
                HStack {
                    TextField("Path", text: $testText, prompt: Text("src/api/read.rs:120"))
                        .font(.body.monospaced())
                        .onSubmit { Task { await test() } }
                    Button("Find") { Task { await test() } }
                }
                if !testResult.isEmpty {
                    Text(testResult).font(.caption.monospaced()).foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
                LabeledContent("Last copy", value: clip.lastEvent)
            }
        }
        .formStyle(.grouped)
        .onAppear { clip.refreshAccess() }
    }

    private var accessStatus: CheckStatus {
        switch clip.access {
        case .allowed: CheckStatus(level: .ok, detail: clip.access.label)
        case .denied: CheckStatus(level: .attention, detail: clip.access.label)
        case .unknown: CheckStatus(level: .pending, detail: clip.access.label)
        case .ask, .notAsked: CheckStatus(level: .pending, detail: clip.access.label)
        }
    }

    private var runningApps: [String] {
        let ids = NSWorkspace.shared.runningApplications
            .filter { $0.activationPolicy == .regular }
            .compactMap(\.bundleIdentifier)
            .filter { !clip.terminalApps.contains($0) }
        return Array(Set(ids)).sorted { appName($0).localizedCaseInsensitiveCompare(appName($1)) == .orderedAscending }
    }

    @MainActor
    private func test() async {
        guard !testText.isEmpty else { return }
        let (res, _) = await clip.handle(text: testText, frontBundleID: Permissions.terminalBundleID,
                                         frontIsTerminal: true, act: false)
        guard let res else { testResult = "Not a path"; return }
        testResult = switch res.outcome {
        case .none: "No match"
        default: res.matches.map { "\($0.path)  (\($0.base?.source ?? "absolute"))" }.joined(separator: "\n")
        }
    }
}

private func isInstalled(_ id: String) -> Bool {
    NSWorkspace.shared.urlForApplication(withBundleIdentifier: id) != nil
}

private func appName(_ id: String) -> String {
    NSWorkspace.shared.urlForApplication(withBundleIdentifier: id)
        .map { FileManager.default.displayName(atPath: $0.path).replacingOccurrences(of: ".app", with: "") } ?? id
}

private struct AppLabel: View {
    let bundleID: String
    var body: some View {
        let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID)
        HStack(spacing: 8) {
            if let url {
                Image(nsImage: NSWorkspace.shared.icon(forFile: url.path)).resizable().frame(width: 18, height: 18)
            } else {
                Image(systemName: "app.dashed").frame(width: 18, height: 18).foregroundStyle(.tertiary)
            }
            Text(appName(bundleID))
            if url == nil { Text("not installed").font(.caption).foregroundStyle(.tertiary) }
        }
    }
}

extension SettingsLink {
    /// Privacy & Security → Paste (macOS 15.4+).
    static let paste = "x-apple.systempreferences:com.apple.preference.security?Privacy_Pasteboard"
}
