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
        var req = API.request("/api/status")
        req.timeoutInterval = 1.5
        if let (_, resp) = try? await URLSession.shared.data(for: req),
           let http = resp as? HTTPURLResponse {
            return http.statusCode == 200
        }
        return false
    }

    private func launch() {
        guard let bin = Bundle.main.executableURL?
            .deletingLastPathComponent()
            .appendingPathComponent("rsd-daemon"),
            FileManager.default.isExecutableFile(atPath: bin.path)
        else { return }

        try? FileManager.default.createDirectory(at: stateDir, withIntermediateDirectories: true)
        copyBundledPlugins()
        let p = Process()
        p.executableURL = bin
        p.arguments = ["watch", NSHomeDirectory(), "--state", stateDir.path]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = (try? FileHandle(forWritingTo: logFile())) ?? FileHandle.nullDevice
        try? p.run()
        process = p
    }

    /// Copy bundled .wasm extractor plugins into <state>/plugins so the daemon
    /// loads them. Overwrites (bundle is the source of truth for shipped ones).
    private func copyBundledPlugins() {
        guard let src = Bundle.main.resourceURL?.appendingPathComponent("plugins") else { return }
        let dst = stateDir.appendingPathComponent("plugins")
        try? FileManager.default.createDirectory(at: dst, withIntermediateDirectories: true)
        let items = (try? FileManager.default.contentsOfDirectory(at: src, includingPropertiesForKeys: nil)) ?? []
        for wasm in items where wasm.pathExtension == "wasm" {
            let target = dst.appendingPathComponent(wasm.lastPathComponent)
            try? FileManager.default.removeItem(at: target)
            try? FileManager.default.copyItem(at: wasm, to: target)
        }
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
