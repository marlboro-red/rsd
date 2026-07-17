// The daemon's localhost API, gated by a loopback token the daemon writes to
// ~/Library/Application Support/rsd/http.token (0600). We read it and present
// it on every request; a web page can't read that file, so it can't reach us.

import Foundation

enum API {
    static let base = "http://127.0.0.1:5871"

    static var token: String {
        let path = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("rsd/http.token")
        return (try? String(contentsOf: path, encoding: .utf8))?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    }

    /// Build a URL for `path` with extra query items. Authentication stays in
    /// a header so the bearer secret never enters URL logs or diagnostics.
    static func url(_ path: String, _ items: [URLQueryItem] = []) -> URL {
        var comps = URLComponents(string: base + path)!
        comps.queryItems = items
        return comps.url!
    }

    static func request(_ path: String, _ items: [URLQueryItem] = []) -> URLRequest {
        var request = URLRequest(url: url(path, items))
        request.setValue(token, forHTTPHeaderField: "X-RSD-Token")
        return request
    }
}
