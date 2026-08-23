import AVFoundation
import CoreImage
import UIKit

/// Pushes every captured frame to connected browsers as multipart JPEG.
/// Unthrottled: conversion is skipped only when the previous one is still
/// in flight (natural backpressure), otherwise frames go out immediately.
final class MJPEGStreamer {
    static let shared = MJPEGStreamer()

    private let lock = NSLock()
    private var conns: [NWConnection] = []
    private var converting = false
    private let ciContext = CIContext()

    var hasViewers: Bool {
        lock.lock(); defer { lock.unlock() }
        return !conns.isEmpty
    }

    /// Registers a browser connection and starts streaming to it.
    func serve(_ conn: NWConnection) {
        conn.stateUpdateHandler = { [weak self] state in
            switch state {
            case .cancelled, .failed:
                self?.remove(conn)
                conn.stateUpdateHandler = nil
            default:
                break   // .waiting etc. is normal — never kill the stream for it
            }
        }
        lock.lock(); conns.append(conn); lock.unlock()
        print("[ius] browser viewer connected (\(conns.count) total)")

        conn.send(content: Data("HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=iusframe\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n".utf8),
                  completion: .contentProcessed { error in
                      if error != nil { conn.cancel() }
                  })
    }

    private func remove(_ conn: NWConnection) {
        lock.lock()
        conns.removeAll { $0 === conn }
        let n = conns.count
        lock.unlock()
        print("[ius] browser viewer disconnected (\(n) total)")
    }

    /// Called from the SCK sample callback.
    func publish(sampleBuffer: CMSampleBuffer) {
        guard hasViewers else { return }

        lock.lock()
        if converting {
            lock.unlock()
            return                          // previous conversion still in flight → drop this frame
        }
        converting = true
        lock.unlock()

        // Create the CIImage synchronously so the pixel buffer is retained,
        // then do the expensive render/encode off the SCK callback path.
        let ci = CIImage(cvPixelBuffer: CMSampleBufferGetImageBuffer(sampleBuffer)!)

        DispatchQueue.global().async { [weak self] in
            guard let self else { return }
            defer {
                self.lock.lock(); self.converting = false; self.lock.unlock()
            }
            guard let cg = self.ciContext.createCGImage(ci, from: ci.extent),
                  let data = UIImage(cgImage: cg).jpegData(compressionQuality: 0.5) else { return }

            var payload = Data("--iusframe\r\nContent-Type: image/jpeg\r\nContent-Length: \(data.count)\r\n\r\n".utf8)
            payload.append(data)
            payload.append(Data("\r\n".utf8))

            self.lock.lock(); let targets = self.conns; self.lock.unlock()
            for c in targets where c.state == .ready {
                c.send(content: payload, completion: .contentProcessed { _ in })
            }
        }
    }
}
