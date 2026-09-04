import AVFoundation
import CoreImage
import UIKit

struct StreamConfig {
    var maxFps: Double = 0        // 0 = uncapped
    var scale: CGFloat = 1.0      // output resolution factor (0.05…1)
    var quality: CGFloat = 0.5    // JPEG quality (0.05…1)

    var key: String { "\(maxFps)|\(scale)|\(quality)" }

    mutating func applyParam(_ k: String, _ v: Double?) {
        guard let v else { return }
        switch k.lowercased() {
        case "fps", "maxfps":
            maxFps = max(0, v)
        case "scale":
            if v >= 0.05 && v <= 1 { scale = CGFloat(v) }
        case "q", "quality":
            if v >= 0.05 && v <= 1 { quality = CGFloat(v) }
        default:
            break
        }
    }

    static func parse(fromPath path: String, fallback: StreamConfig) -> StreamConfig {
        var c = fallback
        guard let qIdx = path.firstIndex(of: "?") else { return c }
        for pair in path[path.index(after: qIdx)...].split(separator: "&") {
            let kv = pair.split(separator: "=", maxSplits: 1)
            guard kv.count == 2, let v = Double(kv[1]) else { continue }
            c.applyParam(String(kv[0]), v)
        }
        return c
    }
}

/// Pushes captured frames to connected browsers as multipart JPEG.
/// Per-viewer settings come from the /stream URL:  /stream?fps=15&scale=0.5&q=0.4
/// Unthrottled by default; frames are dropped only while the previous
/// conversion is still in flight (natural backpressure).
final class MJPEGStreamer {
    static let shared = MJPEGStreamer()

    private struct Client {
        let conn: NWConnection
        let cfg: StreamConfig
        var pendingBytes: Int        // owes the socket this much; reset when done
    }

    private let lock = NSLock()
    private var clients: [Client] = []
    private var converting = false
    private var lastSent: [String: DispatchTime] = [:]
    private var lastConfig = MJPEGStreamer.loadPersisted()
    private let ciContext = CIContext()
    /// Per-connection bytes in flight, used to impose backpressure caps.
    private var pendingBytesByConn: [ObjectIdentifier: Int] = [:]

    /// Absolute ceiling on bytes buffered for one slow client before we
    /// stop feeding it (prevents jetsam/OOM kill).
    private static let maxPendingBytes = 4 * 1024 * 1024

    var hasViewers: Bool {
        lock.lock(); defer { lock.unlock() }
        return !clients.isEmpty
    }

    /// Tuned values survive app relaunches — no need to re-append URL params.
    func persist(_ cfg: StreamConfig) {
        let d = UserDefaults.standard
        d.set(cfg.maxFps, forKey: "ius.stream.fps")
        d.set(Double(cfg.scale), forKey: "ius.stream.scale")
        d.set(Double(cfg.quality), forKey: "ius.stream.quality")
    }

    static func loadPersisted() -> StreamConfig {
        let d = UserDefaults.standard
        var c = StreamConfig()
        if d.object(forKey: "ius.stream.fps") != nil {
            c.maxFps = d.double(forKey: "ius.stream.fps")
            c.scale = CGFloat(d.double(forKey: "ius.stream.scale"))
            c.quality = CGFloat(d.double(forKey: "ius.stream.quality"))
        }
        return c
    }

    /// Registers a browser connection; settings parsed from the request path.
    func serve(_ conn: NWConnection, path: String) {
        lock.lock(); let fallback = lastConfig; lock.unlock()
        let cfg = StreamConfig.parse(fromPath: path, fallback: fallback)
        lock.lock(); lastConfig = cfg; lock.unlock()
        persist(cfg)

        conn.stateUpdateHandler = { [weak self] state in
            switch state {
            case .cancelled, .failed:
                self?.remove(conn)
                conn.stateUpdateHandler = nil
            default:
                break   // .waiting etc. is normal — never kill the stream for it
            }
        }
        lock.lock(); clients.append(Client(conn: conn, cfg: cfg)); lock.unlock()
        print("[ius] browser viewer connected (fps=\(cfg.maxFps == 0 ? "max" : "\(Int(cfg.maxFps))"), scale=\(cfg.scale), q=\(cfg.quality), \(clients.count) total)")

        conn.send(content: Data("HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=iusframe\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n".utf8),
                  completion: .contentProcessed { error in
                      if error != nil { conn.cancel() }
                  })
    }

    private func remove(_ conn: NWConnection) {
        lock.lock()
        clients.removeAll { $0.conn === conn }
        pendingBytesByConn.removeValue(forKey: ObjectIdentifier(conn))
        let n = clients.count
        lock.unlock()
        print("[ius] browser viewer disconnected (\(n) total)")
    }

    /// True if we can queue `bytes` more bytes to this connection right now.
    private func canSend(to conn: NWConnection, bytes: Int) -> Bool {
        let key = ObjectIdentifier(conn)
        return (pendingBytesByConn[key] ?? 0) + bytes <= Self.maxPendingBytes
    }

    private func trackSend(_ conn: NWConnection, bytes: Int) {
        let key = ObjectIdentifier(conn)
        pendingBytesByConn[key] = (pendingBytesByConn[key] ?? 0) + bytes
    }

    private func trackFlush(_ conn: NWConnection, bytes: Int) {
        let key = ObjectIdentifier(conn)
        pendingBytesByConn[key] = max(0, (pendingBytesByConn[key] ?? 0) - bytes)
    }

    /// Called from the SCK sample callback.
    /// Memory-safe conversion: frames are braided through a *bound* work queue,
    /// so clients that stall can't pile allocations onto the heap.
    func publish(sampleBuffer: CMSampleBuffer) {
        guard hasViewers else { return }

        lock.lock()
        if converting {
            lock.unlock()
            return                          // previous conversion still in flight → drop this frame
        }
        converting = true

        // snapshot viewer groups by identical config
        var groups: [String: (cfg: StreamConfig, conns: [NWConnection])] = [:]
        for c in clients {
            guard c.conn.state == .ready else { continue }
            var g = groups[c.cfg.key]
            if g == nil { g = (c.cfg, []) }
            g!.conns.append(c.conn)
            groups[c.cfg.key] = g!
        }
        lock.unlock()

        guard !groups.isEmpty else {
            lock.lock(); converting = false; lock.unlock()
            return
        }

        // Retain the pixel buffer synchronously; heavy work off the SCK callback.
        let ci = CIImage(cvPixelBuffer: CMSampleBufferGetImageBuffer(sampleBuffer)!)

        DispatchQueue.global().async { [weak self] in
            guard let self else { return }
            defer {
                self.lock.lock(); self.converting = false; self.lock.unlock()
            }

            for (key, group) in groups {
                if group.cfg.maxFps > 0 {
                    let now = DispatchTime.now()
                    self.lock.lock()
                    let last = self.lastSent[key]
                    self.lock.unlock()
                    if let last,
                       Double(now.uptimeNanoseconds - last.uptimeNanoseconds) / 1e9
                        < 1.0 / group.cfg.maxFps {
                        continue
                    }
                    self.lock.lock(); self.lastSent[key] = now; self.lock.unlock()
                }

                var scaled = ci
                if group.cfg.scale < 1 {
                    // Proper anti-aliased downscale (windowed-sinc) — replaces the
                    // old affine transform + Gaussian pre-blur hack.
                    scaled = ci.applyingFilter("CILanczosScaleTransform", parameters: [
                        kCIInputScaleKey: group.cfg.scale
                    ])
                    // Restore edge snap lost to resampling so text stays readable.
                    scaled = scaled.applyingFilter("CIUnsharpMask", parameters: [
                        kCIInputRadiusKey: 0.5,
                        kCIInputIntensityKey: 0.3,
                    ])
                }
                guard let cg = self.ciContext.createCGImage(scaled, from: scaled.extent),
                      let data = UIImage(cgImage: cg).jpegData(compressionQuality: group.cfg.quality) else { continue }

                var payload = Data("--iusframe\r\nContent-Type: image/jpeg\r\nContent-Length: \(data.count)\r\n\r\n".utf8)
                payload.append(data)
                payload.append(Data("\r\n".utf8))

                for c in group.conns where c.state == .ready {
                    self.lock.lock()
                    let fits = self.canSend(to: c, bytes: payload.count)
                    if fits { self.trackSend(c, bytes: payload.count) }
                    self.lock.unlock()
                    if !fits {
                        continue   // drop frame for slow viewers -- cap protects the process
                    }
                    let frameSize = payload.count
                    c.send(
                        content: payload,
                        completion: .contentProcessed { [weak self] _ in
                            self?.lock.lock()
                            self?.trackFlush(c, bytes: frameSize)
                            self?.lock.unlock()
                        }
                    )
                }
            }
        }
    }
}
