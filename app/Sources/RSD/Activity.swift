// Activity — a small live HUD over /api/metrics. Opened from the status menu.
// Monochrome, matches the palette: numbers that matter, a convergence light,
// bootstrap progress, and the honest ones the design insists on (semantic lag,
// dedup rate). Polls at 1Hz; it's a pure reader, costs the pipeline nothing.

import SwiftUI

struct Metrics: Decodable {
    struct Throughput: Decodable { let files_indexed: Int; let caes_hits: Int; let caes_misses: Int; let commits: Int }
    struct Health: Decodable { let full_rescans: Int; let worker_crashes: Int; let quarantines: Int; let journal_replays: Int }
    struct Hist: Decodable { let count: Int; let mean_ms: Double; let p50_ms: Double; let p90_ms: Double; let p99_ms: Double }
    struct Freshness: Decodable { let index_latency_ms: Hist; let extract_ms: Hist; let commit_ms: Hist }
    struct Backlog: Decodable { let coalescer_depth: Int; let catalog_entries: Int; let bootstrap_dirs: Int; let bootstrap_done: Bool; let applier_down: Bool }
    let throughput: Throughput
    let health: Health
    let freshness: Freshness
    let backlog: Backlog
}

@MainActor
final class ActivityModel: ObservableObject {
    @Published var m: Metrics?
    @Published var reachable = true
    private var timer: Task<Void, Never>?

    func start() {
        timer?.cancel()
        timer = Task {
            while !Task.isCancelled {
                if let (data, _) = try? await URLSession.shared.data(for: API.request("/api/metrics")),
                   let parsed = try? JSONDecoder().decode(Metrics.self, from: data) {
                    self.m = parsed
                    self.reachable = true
                } else {
                    self.reachable = false
                }
                try? await Task.sleep(nanoseconds: 1_000_000_000)
            }
        }
    }

    func stop() { timer?.cancel() }

    var dedupRate: Double {
        guard let t = m?.throughput else { return 0 }
        let total = t.caes_hits + t.caes_misses
        return total == 0 ? 0 : Double(t.caes_hits) / Double(total)
    }
}

struct ActivityView: View {
    @StateObject private var model = ActivityModel()

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack {
                Text("Index Activity").font(.system(size: 15, weight: .semibold))
                Spacer()
                convergenceLight
            }

            if let m = model.m {
                grid(m)
            } else {
                Text(model.reachable ? "Waiting for the daemon…" : "Daemon unreachable")
                    .font(.system(size: 12)).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .center)
                    .padding(.vertical, 24)
            }
        }
        .padding(20)
        .frame(width: 360)
        .background(.regularMaterial)
        .onAppear { model.start() }
        .onDisappear { model.stop() }
    }

    private var convergenceLight: some View {
        let ok = (model.m?.health.full_rescans ?? 0) == 0
            && (model.m?.health.worker_crashes ?? 0) == 0
            && !(model.m?.backlog.applier_down ?? false)
        return HStack(spacing: 6) {
            Circle().fill(ok ? Color.green : Color.orange).frame(width: 8, height: 8)
            Text(ok ? "converged" : "degraded")
                .font(.system(size: 10, design: .monospaced))
                .foregroundStyle(.secondary)
        }
    }

    private func grid(_ m: Metrics) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            if !m.backlog.bootstrap_done {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Bootstrapping — \(m.backlog.catalog_entries) indexed, \(m.backlog.bootstrap_dirs) folders")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
            }
            row("Files indexed", "\(m.throughput.files_indexed)")
            row("Catalog", "\(m.backlog.catalog_entries) entries")
            row("Live index latency",
                m.freshness.index_latency_ms.count == 0 ? "—"
                : "p50 \(ms(m.freshness.index_latency_ms.p50_ms)) · p99 \(ms(m.freshness.index_latency_ms.p99_ms))")
            row("Commit (incl. embed)",
                m.freshness.commit_ms.count == 0 ? "—" : "p50 \(ms(m.freshness.commit_ms.p50_ms))")
            row("Extraction", m.freshness.extract_ms.count == 0 ? "—" : "p99 \(ms(m.freshness.extract_ms.p99_ms))")
            row("Dedup (copies free)", pct(model.dedupRate))
            if m.health.quarantines > 0 {
                row("Quarantined", "\(m.health.quarantines)", warn: true)
            }
            if m.health.full_rescans > 0 {
                row("Full rescans", "\(m.health.full_rescans)", warn: true)
            }
            if m.backlog.applier_down {
                row("Applier", "down", warn: true)
            }
        }
    }

    private func row(_ label: String, _ value: String, warn: Bool = false) -> some View {
        HStack {
            Text(label).font(.system(size: 12)).foregroundStyle(.secondary)
            Spacer()
            Text(value)
                .font(.system(size: 12, design: .monospaced))
                .foregroundStyle(warn ? .orange : .primary)
        }
    }

    private func ms(_ v: Double) -> String {
        v < 1 ? String(format: "%.0fµs", v * 1000) : String(format: "%.0fms", v)
    }
    private func pct(_ v: Double) -> String { String(format: "%.0f%%", v * 100) }
}
