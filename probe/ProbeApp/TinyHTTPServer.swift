import Foundation
import Network

final class TinyHTTPServer {
    typealias Handler = (_ method: String, _ path: String) -> (Int, [String: Any])

    private var listener: NWListener?
    private let queue = DispatchQueue(label: "ius.http")
    var handler: Handler?
    /// If set, a GET matching this path prefix takes over the connection (e.g. MJPEG).
    var streamHandler: ((NWConnection, String) -> Void)?

    func start(port: UInt16) {
        guard let l = try? NWListener(using: .tcp, on: NWEndpoint.Port(rawValue: port)!) else {
            print("[ius] FAILED to bind :\(port)")
            return
        }
        l.newConnectionHandler = { [weak self] conn in self?.accept(conn) }
        l.start(queue: queue)
        listener = l
    }

    private func accept(_ conn: NWConnection) {
        conn.start(queue: queue)
        receive(conn, Data())
    }

    private func receive(_ conn: NWConnection, _ data: Data) {
        conn.receive(minimumIncompleteLength: 1, maximumLength: 65536) { [weak self] d, _, _, err in
            guard let self else { return }
            var buf = data
            if let d { buf += d }
            if let r = buf.range(of: Data("\r\n\r\n".utf8)) {
                let head = String(decoding: buf[..<r.lowerBound], as: UTF8.self)
                let first = head.split(separator: "\r\n").first.map(String.init) ?? ""
                let parts = first.split(separator: " ")
                let method = parts.isEmpty ? "GET" : String(parts[0])
                let rawPath = parts.count > 1 ? String(parts[1]) : "/"
                let path = String(rawPath.split(separator: "?")[0])
                if let sh = self.streamHandler, path.hasPrefix("/stream") {
                    sh(conn, path)
                    return
                }
                let (status, json) = self.handler?(method, path) ?? (404, ["error": "no handler"])
                let body = (try? JSONSerialization.data(withJSONObject: json)) ?? Data()
                var resp = "HTTP/1.1 \(status)\r\nContent-Type: application/json\r\nContent-Length: \(body.count)\r\nConnection: close\r\n\r\n".data(using: .utf8)!
                resp += body
                conn.send(content: resp, completion: .contentProcessed { _ in conn.cancel() })
            } else if err == nil && buf.count < 16384 {
                self.receive(conn, buf)
            } else {
                conn.cancel()
            }
        }
    }
}