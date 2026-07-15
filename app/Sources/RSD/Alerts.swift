// Standing semantic alerts: saved from the palette (⌘S), watched over SSE,
// delivered as system notifications. "Tell me when something like an invoice
// lands" — the primitive, given a face.

import AppKit
import Foundation
import UserNotifications

struct SavedAlert: Codable, Hashable, Identifiable {
    let query: String
    let threshold: Double
    var id: String { query }
}

@MainActor
final class AlertStore: ObservableObject {
    static let shared = AlertStore()
    @Published private(set) var alerts: [SavedAlert] = []
    private var watchers: [String: Task<Void, Never>] = [:]

    private init() {
        if let data = UserDefaults.standard.data(forKey: "alerts"),
           let saved = try? JSONDecoder().decode([SavedAlert].self, from: data) {
            alerts = saved
        }
        alerts.forEach(watch)
    }

    func add(query: String, threshold: Double = 0.35) {
        let q = query.trimmingCharacters(in: .whitespaces)
        guard !q.isEmpty, !alerts.contains(where: { $0.query == q }) else { return }
        let alert = SavedAlert(query: q, threshold: threshold)
        alerts.append(alert)
        persist()
        watch(alert)
        Notifier.shared.deliver(title: "Watching for “\(q)”", body: "You’ll be notified when similar content appears.")
    }

    func remove(_ alert: SavedAlert) {
        alerts.removeAll { $0 == alert }
        persist()
        watchers.removeValue(forKey: alert.id)?.cancel()
    }

    private func persist() {
        if let data = try? JSONEncoder().encode(alerts) {
            UserDefaults.standard.set(data, forKey: "alerts")
        }
    }

    private func watch(_ alert: SavedAlert) {
        watchers[alert.id]?.cancel()
        watchers[alert.id] = Task {
            while !Task.isCancelled {
                await Self.stream(alert)
                // Daemon restarted or dropped: retry gently.
                try? await Task.sleep(nanoseconds: 5_000_000_000)
            }
        }
    }

    private static func stream(_ alert: SavedAlert) async {
        var comps = URLComponents(string: "http://127.0.0.1:5871/api/alert")!
        comps.queryItems = [
            .init(name: "q", value: alert.query),
            .init(name: "threshold", value: String(alert.threshold)),
        ]
        guard let url = comps.url,
              let (bytes, _) = try? await URLSession.shared.bytes(from: url)
        else { return }
        do {
            for try await line in bytes.lines {
                guard line.hasPrefix("data: "),
                      let data = line.dropFirst(6).data(using: .utf8),
                      let ev = try? JSONDecoder().decode(SseEvent.self, from: data),
                      ev.event == "enter", let path = ev.path
                else { continue }
                await Notifier.shared.deliver(
                    title: "Similar to “\(alert.query)”",
                    body: (path as NSString).lastPathComponent,
                    path: path
                )
            }
        } catch { /* stream ended; outer loop retries */ }
    }
}

struct SseEvent: Decodable {
    let event: String
    let path: String?
}

@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    static let shared = Notifier()
    private var authorized = false

    func setUp() {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { ok, _ in
            Task { @MainActor in self.authorized = ok }
        }
    }

    func deliver(title: String, body: String, path: String? = nil) {
        guard authorized else { return }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        if let path { content.userInfo = ["path": path] }
        UNUserNotificationCenter.current().add(
            UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        )
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        if let path = response.notification.request.content.userInfo["path"] as? String {
            await MainActor.run {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
            }
        }
    }
}
