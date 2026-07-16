// rsd-embed — the embedding sidecar. Loads Apple's NLContextualEmbedding (a
// transformer that runs on the Neural Engine) once, keeps it resident, and
// answers embedding requests over a tiny binary protocol on stdin/stdout.
//
// Because the model lives in THIS process, the daemon can evict it (kill the
// sidecar) to reclaim memory, and an ANE/model fault can't take the daemon
// down. Behind the daemon's Embedder trait it's just another implementation.
//
// Protocol (little-endian):
//   in:  [len: u32][utf8 text]
//   out: [dim: u32][dim × f32]      (mean-pooled, L2-normalized; dim=0 on error)
// A first line "READY <dim>\n" is written to stdout at startup.

import Foundation
import NaturalLanguage

func readExact(_ n: Int) -> Data? {
    var buf = Data()
    while buf.count < n {
        let chunk = FileHandle.standardInput.readData(ofLength: n - buf.count)
        if chunk.isEmpty { return nil }  // EOF
        buf.append(chunk)
    }
    return buf
}

func writeVec(_ v: [Float]) {
    var out = Data()
    var dim = UInt32(v.count).littleEndian
    withUnsafeBytes(of: &dim) { out.append(contentsOf: $0) }
    v.forEach { f in
        var le = f.bitPattern.littleEndian
        withUnsafeBytes(of: &le) { out.append(contentsOf: $0) }
    }
    FileHandle.standardOutput.write(out)
}

guard #available(macOS 14.0, *), let emb = NLContextualEmbedding(language: .english) else {
    FileHandle.standardError.write("rsd-embed: NLContextualEmbedding unavailable\n".data(using: .utf8)!)
    exit(1)
}
if !emb.hasAvailableAssets {
    // Bounded wait: on a headless machine assets may never arrive; give up and
    // exit so the daemon falls back rather than hanging.
    let sem = DispatchSemaphore(value: 0)
    emb.requestAssets { _, _ in sem.signal() }
    if sem.wait(timeout: .now() + 30) == .timedOut {
        FileHandle.standardError.write("rsd-embed: assets unavailable\n".data(using: .utf8)!)
        exit(1)
    }
}
do { try emb.load() } catch {
    FileHandle.standardError.write("rsd-embed: load failed: \(error)\n".data(using: .utf8)!)
    exit(1)
}
let dim = emb.dimension
// FileHandle (not print) — print is fully buffered under a pipe and would
// deadlock the client waiting for READY.
FileHandle.standardOutput.write("READY \(dim)\n".data(using: .utf8)!)

func embed(_ text: String) -> [Float] {
    var sum = [Float](repeating: 0, count: dim)
    var n = 0
    guard let result = try? emb.embeddingResult(for: text, language: .english) else {
        return sum
    }
    result.enumerateTokenVectors(in: text.startIndex..<text.endIndex) { vec, _ in
        for i in 0..<min(dim, vec.count) { sum[i] += Float(vec[i]) }
        n += 1
        return true
    }
    if n > 0 { for i in 0..<dim { sum[i] /= Float(n) } }
    var norm: Float = 0
    for x in sum { norm += x * x }
    norm = norm.squareRoot()
    if norm > 0 { for i in 0..<dim { sum[i] /= norm } }
    return sum
}

while let header = readExact(4) {
    let len = header.withUnsafeBytes { $0.load(as: UInt32.self) }.littleEndian
    guard len <= 16 * 1024 * 1024, let body = readExact(Int(len)) else { break }
    let text = String(data: body, encoding: .utf8) ?? ""
    writeVec(embed(text))
}
