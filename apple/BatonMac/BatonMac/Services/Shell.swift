import Foundation

/// Run a command off the main thread with a hard timeout. Never call from
/// the main actor — callers use Task.detached.
enum Shell {
    struct Result {
        var status: Int32
        var stdout: String
        var stderr: String
        var timedOut: Bool
    }

    static func run(_ path: String, _ args: [String], timeout: TimeInterval) -> Result {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: path)
        p.arguments = args
        let out = Pipe(), err = Pipe()
        p.standardOutput = out
        p.standardError = err
        p.standardInput = FileHandle.nullDevice
        do { try p.run() } catch {
            return Result(status: -1, stdout: "", stderr: error.localizedDescription, timedOut: false)
        }
        // Drain pipes concurrently so a large output can't deadlock the child.
        var outData = Data(), errData = Data()
        let group = DispatchGroup()
        group.enter()
        DispatchQueue.global().async { outData = out.fileHandleForReading.readDataToEndOfFile(); group.leave() }
        group.enter()
        DispatchQueue.global().async { errData = err.fileHandleForReading.readDataToEndOfFile(); group.leave() }
        let deadline = Date().addingTimeInterval(timeout)
        while p.isRunning && Date() < deadline { usleep(20_000) }
        var timedOut = false
        if p.isRunning { p.terminate(); timedOut = true }
        _ = group.wait(timeout: .now() + 1)
        return Result(
            status: timedOut ? -2 : p.terminationStatus,
            stdout: String(decoding: outData, as: UTF8.self),
            stderr: String(decoding: errData, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines),
            timedOut: timedOut)
    }
}
