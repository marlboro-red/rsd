// The app owns the daemon: at launch, if no daemon answers, start the bundled
// one watching the whole home folder (the scanner's built-in exclusions keep
// Library/caches/VCS churn out). State lives in ~/Library/Application
// Support/rsd — inside home, but under the excluded tree, so it never feeds
// back into the index.

import Foundation
import AppKit

@MainActor
final class DaemonManager {
    static let shared = DaemonManager()
    private var process: Process?

    var stateDir: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("rsd")
    }

    func ensureRunning() {
        Task {
            if await self.responds() { return }
            self.launch()
        }
    }

    private func responds() async -> Bool {
        guard let url = URL(string: "http://127.0.0.1:5871/api/status") else { return false }
        var req = URLRequest(url: url)
        req.timeoutInterval = 1.5
        return (try? await URLSession.shared.data(for: req)) != nil
    }

    private func launch() {
        guard let bin = Bundle.main.executableURL?
            .deletingLastPathComponent()
            .appendingPathComponent("rsd-daemon"),
            FileManager.default.isExecutableFile(atPath: bin.path)
        else { return }

        try? FileManager.default.createDirectory(at: stateDir, withIntermediateDirectories: true)
        let p = Process()
        p.executableURL = bin
        p.arguments = ["watch", NSHomeDirectory(), "--state", stateDir.path]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = try? FileHandle(
            forWritingTo: logFile()
        ) ?? FileHandle.nullDevice
        try? p.run()
        process = p
    }

    private func logFile() -> URL {
        let url = stateDir.appendingPathComponent("daemon.log")
        FileManager.default.createFile(atPath: url.path, contents: nil)
        return url
    }

    func stop() {
        process?.terminate()
        process = nil
    }
}
