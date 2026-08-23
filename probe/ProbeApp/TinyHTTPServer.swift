import Foundation
import CryptoKit
import Network

final class TinyHTTPServer {
    typealias Handler = (_ method: String, _ path: String) -> (Int, [String: Any])
    typealias RawHandler = (_ method: String, _ path: String) -> (Int, String, Data)?
    typealias WebSocketHandler = (_ conn: NWConnection, _ headers: [String: String]) -> Void

    private var listener: NWListener?
    private let queue = DispatchQueue(label: "ius.http")
    var handler: Handler?
    /// Raw byte responses (html/png/etc). Return nil to fall through to `handler`.
    var rawHandler: RawHandler?
    /// If set, a GET with `Upgrade: websocket` on this path takes over the connection.
    var webSocketHandler: WebSocketHandler?
    /// MJPEG-style takeover for an exact request path.
    var streamHandler: ((NWConnection, String) -> Void)?

    private static let wsGUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

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
            if err != nil { conn.cancel(); return }
            var buf = data
            if let d { buf += d }
            guard let r = buf.range(of: Data("\r\n\r\n".utf8)) else {
                if buf.count < 16384 { self.receive(conn, buf) } else { conn.cancel() }
                return
            }
            let head = String(decoding: buf[..<r.lowerBound], as: UTF8.self)
            let lines = head.split(separator: "\r\n").map(String.init)
            let first = lines.first ?? ""
            let parts = first.split(separator: " ")
            let method = parts.isEmpty ? "GET" : String(parts[0])
            let rawPath = parts.count > 1 ? String(parts[1]) : "/"
            let path = String(rawPath.split(separator: "?")[0])

            var headers: [String: String] = [:]
            for line in lines.dropFirst() {
                guard let ci = line.firstIndex(of: ":") else { continue }
                let k = String(line[..<ci]).trimmingCharacters(in: .whitespaces).lowercased()
                let v = String(line[line.index(after: ci)...]).trimmingCharacters(in: .whitespaces)
                headers[k] = v
            }

            // MJPEG-style exact-path takeover
            if method == "GET", path == "/stream", let sh = self.streamHandler {
                sh(conn, rawPath)
                return
            }

            // WebSocket upgrade
            if method == "GET",
               headers["upgrade"]?.lowercased() == "websocket",
               path.hasPrefix("/stream.ws"), let sh = self.webSocketHandler {
                let accept = Self.wsAccept(key: headers["sec-websocket-key"] ?? "")
                let resp = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n" +
                           "Connection: Upgrade\r\nSec-WebSocket-Accept: \(accept)\r\n\r\n"
                conn.send(content: Data(resp.utf8), completion: .contentProcessed { _ in
                    sh(conn, headers)
                })
                return
            }

            // Raw byte routes (html pages etc.)
            if method == "GET", let rh = self.rawHandler,
               let (status, ctype, body) = rh(method, path) {
                var resp = "HTTP/1.1 \(status)\r\nContent-Type: \(ctype)\r\n" +
                           "Content-Length: \(body.count)\r\nConnection: close\r\n\r\n"
                var out = Data(resp.utf8)
                out.append(body)
                conn.send(content: out, completion: .contentProcessed { _ in conn.cancel() })
                return
            }

            let (status, json) = self.handler?(method, path) ?? (404, ["error": "not found"])
            let body = (try? JSONSerialization.data(withJSONObject: json)) ?? Data()
            var resp = "HTTP/1.1 \(status)\r\nContent-Type: application/json\r\nContent-Length: \(body.count)\r\nConnection: close\r\n\r\n"
            var out = Data(resp.utf8)
            out.append(body)
            conn.send(content: out, completion: .contentProcessed { _ in conn.cancel() })
        }
    }

    static func wsAccept(key: String) -> String {
        wsSHA1(Data((key + wsGUID).utf8)).base64EncodedString()
    }
}

import CryptoKit

private func wsSHA1(_ data: Data) -> Data {
    Data(Insecure.SHA1.hash(data: data))
}

/// Minimal server-side WebSocket: binary/text sends, close/ping handling.
final class WebSocketConn {
    let conn: NWConnection
    private let queue = DispatchQueue(label: "ius.ws")
    private var buffer = Data()
    private var closed = false
    private let sendLock = NSLock()
    let onClose: () -> Void

    init(conn: NWConnection, onClose: @escaping () -> Void) {
        self.conn = conn
        self.onClose = onClose
        recvLoop()
    }

    deinit { /* conn cancelled by close() */ }

    func close() {
        guard !closed else { return }
        closed = true
        conn.cancel()
        onClose()
    }

    private func failClose() {
        guard !closed else { return }
        closed = true
        conn.cancel()
        onClose()
    }

    func sendBinary(_ data: Data) {
        sendFrame(opcode: 0x2, payload: data)
    }

    func sendText(_ text: String) {
        sendFrame(opcode: 0x1, payload: Data(text.utf8))
    }

    private func sendFrame(opcode: UInt8, payload: Data) {
        sendLock.lock()
        defer { sendLock.unlock() }
        guard !closed else { return }
        var frame = Data([0x80 | opcode])
        let n = payload.count
        if n < 126 {
            frame.append(UInt8(n))
        } else if n <= Int(UInt16.max) {
            frame.append(126)
            frame.append(contentsOf: withUnsafeBytes(of: UInt16(n).bigEndian) { Data($0) })
        } else {
            frame.append(127)
            frame.append(contentsOf: withUnsafeBytes(of: UInt64(n).bigEndian) { Data($0) })
        }
        frame.append(payload)
        conn.send(content: frame, completion: .contentProcessed { [weak self] error in
            if error != nil { self?.failClose() }
        })
    }

    private func recvLoop() {
        guard !closed else { return }
        conn.receive(minimumIncompleteLength: 1, maximumLength: 262144) { [weak self] d, _, _, err in
            guard let self else { return }
            if err != nil { self.failClose(); return }
            if let d { self.buffer += d }
            self.processFrames()
            if self.closed { return }
            self.recvLoop()
        }
    }

    private func processFrames() {
        while true {
            guard let (opcode, payload, consumed) = parseFrame() else { break }
            buffer.removeSubrange(..<consumed)
            switch opcode {
            case 0x8:                       // close
                sendFrame(opcode: 0x8, payload: Data())
                close()
                return
            case 0x9:                       // ping -> pong
                sendFrame(opcode: 0xA, payload: payload)
            default:                        // text/binary/continuation from client: ignored
                break
            }
        }
    }

    private func parseFrame() -> (UInt8, Data, Int)? {
        let b = buffer
        guard b.count >= 2 else { return nil }
        let opcode = b[0] & 0x7F
        let masked = (b[1] & 0x80) != 0
        var len = Int(b[1] & 0x7F)
        var off = 2
        if len == 126 {
            guard b.count >= 4 else { return nil }
            len = Int(b[2]) << 8 | Int(b[3]); off = 4
        } else if len == 127 {
            guard b.count >= 10 else { return nil }
            len = 0
            for i in 2..<10 { len = len << 8 | Int(b[i]) }
            off = 10
        }
        var maskKey: [UInt8] = []
        if masked {
            guard b.count >= off + 4 else { return nil }
            maskKey = Array(b[off..<off+4]); off += 4
        }
        guard b.count >= off + len else { return nil }
        var payload = Data(b[off..<off+len])
        if masked, !maskKey.isEmpty {
            for i in 0..<payload.count { payload[i] ^= maskKey[i % 4] }
        }
        return (opcode, payload, off + len)
    }
}
