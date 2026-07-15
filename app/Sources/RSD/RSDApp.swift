// RSD.app — the fusion search instrument.
//
// Design language: a bare floating field that grows into results. Monochrome;
// the only color is the system accent on selection. Type carries hierarchy
// (26pt ultralight query, medium names, SF Mono for paths and data). The
// signature: provenance glyphs — every hybrid result says WHY it surfaced
// (= exact, ≈ meaning, ≋ both). No footer, no chrome, one spring.

import SwiftUI
import AppKit

@main
struct RSDApp: App {
    @NSApplicationDelegateAdaptor(Summoner.self) private var summoner

    var body: some Scene {
        WindowGroup("RSD") {
            SearchView()
                .frame(width: 720)
        }
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentSize)
    }
}

// MARK: - Model

struct Hit: Identifiable, Decodable, Hashable {
    let path: String
    let snippet: String
    let match: String?
    var id: String { path }
    var name: String { (path as NSString).lastPathComponent }
    var dir: String {
        (path as NSString).deletingLastPathComponent
            .replacingOccurrences(of: NSHomeDirectory(), with: "~")
    }
    var glyph: String {
        switch match {
        case "exact": "="
        case "meaning": "≈"
        case "both": "≋"
        default: ""
        }
    }
    var glyphHelp: String {
        switch match {
        case "exact": "Matched exact words"
        case "meaning": "Matched by meaning"
        case "both": "Matched exact words and meaning"
        default: ""
        }
    }
}

struct SearchResponse: Decodable {
    let hits: [Hit]
    let ms: Double
}

enum Mode: String, CaseIterable {
    case hybrid, exact, meaning
    var next: Mode {
        let all = Mode.allCases
        return all[(all.firstIndex(of: self)! + 1) % all.count]
    }
    var apiValue: String {
        switch self {
        case .exact: "lexical"
        case .meaning: "semantic"
        case .hybrid: "hybrid"
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
        let mode = mode.apiValue
        task = Task {
            try? await Task.sleep(nanoseconds: 90_000_000)
            if Task.isCancelled { return }
            guard !q.trimmingCharacters(in: .whitespaces).isEmpty else {
                withAnimation(.spring(duration: 0.28)) {
                    self.hits = []
                    self.latencyMs = nil
                }
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
                withAnimation(.spring(duration: 0.28)) {
                    self.hits = resp.hits
                }
                self.latencyMs = resp.ms
                self.daemonUp = true
                if self.selection == nil || !resp.hits.contains(where: { $0.id == self.selection }) {
                    self.selection = resp.hits.first?.id
                }
            } catch is DecodingError {
                withAnimation(.spring(duration: 0.28)) { self.hits = [] }
            } catch {
                self.daemonUp = false
            }
        }
    }

    var selected: Hit? { hits.first { $0.id == selection } }

    func open(_ hit: Hit?) {
        guard let hit else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: hit.path))
    }

    func reveal(_ hit: Hit?) {
        guard let hit else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: hit.path)])
    }

    func move(_ delta: Int) {
        guard !hits.isEmpty else { return }
        let idx = hits.firstIndex { $0.id == selection } ?? 0
        selection = hits[min(max(idx + delta, 0), hits.count - 1)].id
    }
}

// MARK: - Views

struct SearchView: View {
    @StateObject private var model = SearchModel()
    @FocusState private var focused: Bool

    private let rowHeight: CGFloat = 54
    private var resultsHeight: CGFloat {
        CGFloat(min(model.hits.count, 8)) * rowHeight + (model.hits.isEmpty ? 0 : 12)
    }

    var body: some View {
        VStack(spacing: 0) {
            field
            if !model.daemonUp {
                notice("rsd isn’t running", detail: "rsd-daemon watch <folder>")
            } else if model.hits.isEmpty && !model.query.trimmingCharacters(in: .whitespaces).isEmpty {
                notice("Nothing for “\(model.query)”", detail: nil)
            } else if !model.hits.isEmpty {
                Rectangle().fill(.separator).frame(height: 0.5).opacity(0.5)
                ResultsList(model: model, rowHeight: rowHeight)
                    .frame(height: resultsHeight)
            }
        }
        .background(.regularMaterial)
        .onAppear { focused = true }
    }

    private var field: some View {
        HStack(spacing: 14) {
            TextField("Search your Mac…", text: $model.query)
                .textFieldStyle(.plain)
                .font(.system(size: 26, weight: .ultraLight))
                .kerning(0.2)
                .focused($focused)
                .onChange(of: model.query) { model.search() }
                .onSubmit { model.open(model.selected) }
                .onKeyPress(.downArrow) { model.move(1); return .handled }
                .onKeyPress(.upArrow) { model.move(-1); return .handled }
                .onKeyPress(.tab) {
                    model.mode = model.mode.next
                    model.search()
                    return .handled
                }
                .onKeyPress(.escape) {
                    if model.query.isEmpty { NSApp.hide(nil) } else { model.query = "" }
                    return .handled
                }

            VStack(alignment: .trailing, spacing: 3) {
                Button(model.mode.rawValue) {
                    model.mode = model.mode.next
                    model.search()
                }
                .buttonStyle(.plain)
                .font(.system(size: 11, weight: .medium, design: .monospaced))
                .foregroundStyle(Color.accentColor)
                .help("Search mode — click or press Tab to switch")

                if let ms = model.latencyMs, !model.hits.isEmpty {
                    Text(ms < 1 ? String(format: "%.0f µs", ms * 1000) : String(format: "%.1f ms", ms))
                        .font(.system(size: 10, design: .monospaced))
                        .foregroundStyle(.quaternary)
                        .contentTransition(.numericText())
                }
            }
        }
        .padding(.horizontal, 22)
        .frame(height: 68)
    }

    private func notice(_ title: String, detail: String?) -> some View {
        VStack(spacing: 6) {
            Text(title).font(.system(size: 12)).foregroundStyle(.secondary)
            if let detail {
                Text(detail)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.tertiary)
                    .textSelection(.enabled)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.top, 2)
        .padding(.bottom, 18)
    }
}

struct ResultsList: View {
    @ObservedObject var model: SearchModel
    let rowHeight: CGFloat

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(model.hits) { hit in
                        HitRow(hit: hit, selected: hit.id == model.selection, height: rowHeight)
                            .id(hit.id)
                            .onTapGesture(count: 2) { model.open(hit) }
                            .onTapGesture { model.selection = hit.id }
                            .contextMenu {
                                Button("Open") { model.open(hit) }
                                Button("Reveal in Finder") { model.reveal(hit) }
                                Button("Copy Path") {
                                    NSPasteboard.general.clearContents()
                                    NSPasteboard.general.setString(hit.path, forType: .string)
                                }
                            }
                    }
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
            }
            .onChange(of: model.selection) {
                if let sel = model.selection {
                    withAnimation(.easeOut(duration: 0.12)) { proxy.scrollTo(sel) }
                }
            }
        }
        .background(
            // ⌘⏎ reveal, invisible but reachable.
            Button("") { model.reveal(model.selected) }
                .keyboardShortcut(.return, modifiers: .command)
                .opacity(0)
        )
    }
}

struct HitRow: View {
    let hit: Hit
    let selected: Bool
    let height: CGFloat

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            Image(nsImage: NSWorkspace.shared.icon(forFile: hit.path))
                .resizable()
                .frame(width: 30, height: 30)
            VStack(alignment: .leading, spacing: 2.5) {
                HStack(spacing: 8) {
                    Text(hit.name)
                        .font(.system(size: 13, weight: .medium))
                        .lineLimit(1)
                    Text(hit.dir)
                        .font(.system(size: 10.5, design: .monospaced))
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                if !hit.snippet.isEmpty {
                    Text(hit.snippet)
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 8)
            if !hit.glyph.isEmpty {
                Text(hit.glyph)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.tertiary)
                    .help(hit.glyphHelp)
            }
        }
        .padding(.horizontal, 12)
        .frame(height: height)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(selected ? Color.accentColor.opacity(0.15) : .clear)
        )
        .contentShape(Rectangle())
    }
}
