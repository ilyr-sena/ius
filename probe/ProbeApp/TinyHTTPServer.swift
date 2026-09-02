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
            let body = Data(buf[r.upperBound...])
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

            // POST with a Content-Length we haven't fully received yet — keep reading.
            if method == "POST",
               let cl = headers["content-length"].flatMap(Int.init), body.count < cl {
                self.receive(conn, buf)
                return
            }

            self.dispatch(conn, method: method, path: path, rawPath: rawPath, headers: headers, body: body)
        }
    }

    private func dispatch(_ conn: NWConnection, method: String, path: String, rawPath: String,
                          headers: [String: String], body: Data) {
            // MJPEG-style exact-path takeover
            if method == "GET", path == "/stream", let sh = self.streamHandler {
                sh(conn, rawPath)
                return
            }

            // WebSocket upgrade (exact match only — avoid grabbing unrelated paths)
            if method == "GET",
               headers["upgrade"]?.lowercased() == "websocket",
               path == "/stream.ws", let sh = self.webSocketHandler {
                let accept = Self.wsAccept(key: headers["sec-websocket-key"] ?? "")
                let resp = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n" +
                           "Connection: Upgrade\r\nSec-WebSocket-Accept: \(accept)\r\n\r\n"
                conn.send(content: Data(resp.utf8), completion: .contentProcessed { _ in
                    sh(conn, headers)
                })
                return
            }

            // POST body JSON routes
            if method == "POST", path == "/stream/tuning" {
                let obj = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] ?? [:]
                var t = H264Stream.shared.currentTuning()
                if let v = obj["bitrateMbps"] as? NSNumber { t.bitrateMbps = v.doubleValue }
                if let v = obj["maxFps"] as? NSNumber { t.maxFps = v.doubleValue }
                if let v = obj["scale"] as? NSNumber { t.scale = v.doubleValue }
                if let v = obj["keyframeSeconds"] as? NSNumber { t.keyframeSeconds = v.doubleValue }
                let applied = H264Stream.shared.applyTuning(t)
                let out: [String: Any] = [
                    "ok": true, "bitrateMbps": applied.bitrateMbps, "maxFps": applied.maxFps,
                    "scale": applied.scale, "keyframeSeconds": applied.keyframeSeconds,
                ]
                let b = (try? JSONSerialization.data(withJSONObject: out)) ?? Data()
                let resp = "HTTP/1.1 200\r\nContent-Type: application/json\r\nContent-Length: \(b.count)\r\n\r\n"
                var o = Data(resp.utf8); o.append(b)
                conn.send(content: o, completion: .contentProcessed { _ in conn.cancel() })
                return
            }

            // Raw byte routes (html pages etc.)
            if method == "GET", let rh = self.rawHandler,
               let (status, ctype, rawBody) = rh(method, path) {
                var resp = "HTTP/1.1 \(status)\r\nContent-Type: \(ctype)\r\n" +
                           "Content-Length: \(rawBody.count)\r\nConnection: close\r\n\r\n"
                var out = Data(resp.utf8)
                out.append(rawBody)
                conn.send(content: out, completion: .contentProcessed { _ in conn.cancel() })
                return
            }

            let (status, json) = self.handler?(method, path) ?? (404, ["error": "not found"])
            let outBody = (try? JSONSerialization.data(withJSONObject: json)) ?? Data()
            var resp = "HTTP/1.1 \(status)\r\nContent-Type: application/json\r\nContent-Length: \(outBody.count)\r\nConnection: close\r\n\r\n"
            var out = Data(resp.utf8)
            out.append(outBody)
            conn.send(content: out, completion: .contentProcessed { _ in conn.cancel() })
    }

    static func wsAccept(key: String) -> String {
        wsSHA1(Data((key + wsGUID).utf8)).base64EncodedString()
    }
}

private func wsSHA1(_ data: Data) -> Data {
    Data(Insecure.SHA1.hash(data: data))
}

/// Minimal server-side WebSocket: per-client outbound queue so one slow or
/// dead viewer can never stall other viewers or the encoder pipeline.
final class WebSocketConn {
    let conn: NWConnection
    let onClose: () -> Void
    /// Client text/binary frames: (isText, payload)
    var onMessage: ((Bool, Data) -> Void)?

    private let lock = NSLock()
    private var outbox: [Data] = []
    private var outBytes = 0
    private var flushing = false
    private var closed = false
    private var buffer = Data()

    static let maxOutboxBytes = 6 * 1024 * 1024

    init(conn: NWConnection, onClose: @escaping () -> Void) {
        self.conn = conn
        self.onClose = onClose
        recvLoop()
    }

    /// Queue a websocket binary/text frame. Never blocks the caller.
    func enqueue(opcode: UInt8 = 0x2, payload: Data) {
        lock.lock()
        guard !closed else { lock.unlock(); return }
        var frame = encodeFrame(opcode: opcode, payload: payload)
        outbox.append(frame)
        outBytes += frame.count
        frame = Data()
        let overflow = outBytes > Self.maxOutboxBytes
        lock.unlock()
        if overflow {
            print("[ius] ws outbox overflow - dropping client")
            close()
            return
        }
        pump()
    }

    func sendText(_ t: String) { enqueue(opcode: 0x1, payload: Data(t.utf8)) }
    func sendBinary(_ d: Data) { enqueue(opcode: 0x2, payload: d) }

    func close() {
        lock.lock()
        guard !closed else { lock.unlock(); return }
        closed = true
        outbox = []
        outBytes = 0
        lock.unlock()
        conn.cancel()
        onClose()
    }

    private func encodeFrame(opcode: UInt8, payload: Data) -> Data {
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
        return frame
    }

    private func pump() {
        lock.lock()
        if flushing || closed {
            lock.unlock()
            return
        }
        flushing = true
        let batch = outbox
        outbox = []
        outBytes = 0
        lock.unlock()

        guard !batch.isEmpty else {
            lock.lock(); flushing = false; lock.unlock()
            return
        }
        var all = Data()
        for f in batch { all.append(f) }

        conn.send(content: all, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            self.lock.lock()
            self.flushing = false
            let more = !self.outbox.isEmpty && !self.closed
            self.lock.unlock()
            if error != nil {
                self.close()
                return
            }
            if more { self.pump() }
        })
    }

    private func recvLoop() {
        lock.lock()
        let alive = !closed
        lock.unlock()
        guard alive else { return }
        conn.receive(minimumIncompleteLength: 1, maximumLength: 262144) { [weak self] d, _, _, err in
            guard let self else { return }
            if err != nil { self.close(); return }
            if let d { self.buffer += d }
            self.processFrames()
            self.recvLoop()
        }
    }

    private func processFrames() {
        while true {
            guard let (opcode, payload) = parseFrame() else { break }
            switch opcode {
            case 0x8:                       // close
                enqueue(opcode: 0x8, payload: Data())
                close()
                return
            case 0x9:                       // ping -> pong
                enqueue(opcode: 0xA, payload: payload)
            default:
                onMessage?(opcode == 0x1, payload)
            }
        }
    }

    private func parseFrame() -> (UInt8, Data)? {
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
        return (opcode, payload)
    }
}
