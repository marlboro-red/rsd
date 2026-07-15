// RSD.app — the native search palette over the rsd daemon.
//
// Spotlight-shaped, but honest about being better: sub-millisecond lexical,
// on-device semantic, grounded snippets, live index. Talks to the daemon's
// localhost JSON API (127.0.0.1:5871); everything stays on the machine.

import SwiftUI
import AppKit

@main
struct RSDApp: App {
    var body: some Scene {
        WindowGroup("RSD") {
            SearchView()
                .frame(minWidth: 680, minHeight: 460)
        }
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentSize)
    }
}

struct Hit: Identifiable, Decodable, Hashable {
    let path: String
    let snippet: String
    var id: String { path }
    var name: String { (path as NSString).lastPathComponent }
    var dir: String {
        (path as NSString).deletingLastPathComponent
            .replacingOccurrences(of: NSHomeDirectory(), with: "~")
    }
}

struct SearchResponse: Decodable {
    let hits: [Hit]
    let ms: Double
}

enum Mode: String, CaseIterable, Identifiable {
    case hybrid, lexical, semantic
    var id: String { rawValue }
    var label: String {
        switch self {
        case .hybrid: "Hybrid"
        case .lexical: "Exact"
        case .semantic: "Meaning"
        }
    }
}

@MainActor
final class SearchModel: ObservableObject {
    @Published var query = ""
    @Published var mode: Mode = .hybrid
    @Published var hits: [Hit] = []
    @Published var latencyMs: Double?
    @Published var daemonUp = true
    @Published var selection: Hit.ID?
    private var task: Task<Void, Never>?

    func search() {
        task?.cancel()
        let q = query
        let mode = mode.rawValue
        task = Task {
            // Debounce: results-as-you-type without hammering on every glyph.
            try? await Task.sleep(nanoseconds: 90_000_000)
            if Task.isCancelled { return }
            guard !q.trimmingCharacters(in: .whitespaces).isEmpty else {
                self.hits = []
                self.latencyMs = nil
                return
            }
            var comps = URLComponents(string: "http://127.0.0.1:5871/api/search")!
            comps.queryItems = [
                .init(name: "q", value: q),
                .init(name: "mode", value: mode),
                .init(name: "limit", value: "40"),
            ]
            do {
                let (data, _) = try await URLSession.shared.data(from: comps.url!)
                if Task.isCancelled { return }
                let resp = try JSONDecoder().decode(SearchResponse.self, from: data)
                self.hits = resp.hits
                self.latencyMs = resp.ms
                self.daemonUp = true
                if self.selection == nil || !resp.hits.contains(where: { $0.id == self.selection }) {
                    self.selection = resp.hits.first?.id
                }
            } catch is DecodingError {
                self.hits = []
            } catch {
                self.daemonUp = false
            }
        }
    }

    func open(_ hit: Hit?) {
        guard let hit else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: hit.path))
    }

    func reveal(_ hit: Hit?) {
        guard let hit else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: hit.path)])
    }

    var selected: Hit? { hits.first { $0.id == selection } }

    func move(_ delta: Int) {
        guard !hits.isEmpty else { return }
        let idx = hits.firstIndex { $0.id == selection } ?? 0
        let next = min(max(idx + delta, 0), hits.count - 1)
        selection = hits[next].id
    }
}

struct SearchView: View {
    @StateObject private var model = SearchModel()
    @FocusState private var searchFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider().opacity(0.4)
            results
            footer
        }
        .background(.ultraThinMaterial)
        .onAppear { searchFocused = true }
    }

    private var header: some View {
        VStack(spacing: 10) {
            HStack(spacing: 12) {
                Image(systemName: "sparkle.magnifyingglass")
                    .font(.system(size: 22, weight: .light))
                    .foregroundStyle(.secondary)
                TextField("Search everything…", text: $model.query)
                    .textFieldStyle(.plain)
                    .font(.system(size: 24, weight: .light))
                    .focused($searchFocused)
                    .onChange(of: model.query) { model.search() }
                    .onSubmit { model.open(model.selected) }
                    .onKeyPress(.downArrow) { model.move(1); return .handled }
                    .onKeyPress(.upArrow) { model.move(-1); return .handled }
                if let ms = model.latencyMs {
                    Text(ms < 1 ? String(format: "%.0f µs", ms * 1000) : String(format: "%.1f ms", ms))
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.tertiary)
                }
            }
            Picker("", selection: $model.mode) {
                ForEach(Mode.allCases) { m in Text(m.label).tag(m) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .frame(width: 300)
            .onChange(of: model.mode) { model.search() }
        }
        .padding(.horizontal, 20)
        .padding(.top, 18)
        .padding(.bottom, 12)
    }

    private var results: some View {
        Group {
            if !model.daemonUp {
                ContentUnavailableView(
                    "Daemon not running",
                    systemImage: "bolt.slash",
                    description: Text("Start it with:  rsd-daemon watch <folder>")
                )
            } else if model.hits.isEmpty && !model.query.isEmpty {
                ContentUnavailableView.search(text: model.query)
            } else if model.hits.isEmpty {
                ContentUnavailableView(
                    "RSD",
                    systemImage: "sparkle.magnifyingglass",
                    description: Text("Sub-millisecond exact search. On-device semantic search. Your files never leave this Mac.")
                )
            } else {
                ScrollViewReader { proxy in
                    List(model.hits, selection: $model.selection) { hit in
                        HitRow(hit: hit)
                            .tag(hit.id)
                            .id(hit.id)
                            .contextMenu {
                                Button("Open") { model.open(hit) }
                                Button("Reveal in Finder") { model.reveal(hit) }
                                Button("Copy Path") {
                                    NSPasteboard.general.clearContents()
                                    NSPasteboard.general.setString(hit.path, forType: .string)
                                }
                            }
                    }
                    .listStyle(.inset)
                    .scrollContentBackground(.hidden)
                    .onChange(of: model.selection) {
                        if let sel = model.selection { proxy.scrollTo(sel) }
                    }
                }
            }
        }
        .frame(maxHeight: .infinity)
    }

    private var footer: some View {
        HStack {
            Text("↩ Open   ⌘↩ Reveal")
                .font(.caption2)
                .foregroundStyle(.tertiary)
            Spacer()
            Text("\(model.hits.count) results")
                .font(.caption2)
                .foregroundStyle(.tertiary)
                .opacity(model.hits.isEmpty ? 0 : 1)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
        .background(
            Button("") { model.reveal(model.selected) }
                .keyboardShortcut(.return, modifiers: .command)
                .opacity(0)
        )
    }
}

struct HitRow: View {
    let hit: Hit

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(nsImage: NSWorkspace.shared.icon(forFile: hit.path))
                .resizable()
                .frame(width: 32, height: 32)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 8) {
                    Text(hit.name).font(.system(size: 13, weight: .medium))
                    Text(hit.dir)
                        .font(.system(size: 11))
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                if !hit.snippet.isEmpty {
                    Text(hit.snippet)
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }
        }
        .padding(.vertical, 3)
    }
}
