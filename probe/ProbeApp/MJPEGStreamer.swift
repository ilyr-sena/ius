import AVFoundation
import CoreImage
import UIKit

/// Serves the live capture as an MJPEG stream consumable by any browser.
final class MJPEGStreamer {
    static let shared = MJPEGStreamer()

    private let lock = NSLock()
    private var latestJPEG: Data?
    private var viewers = 0
    private var lastPublish: DispatchTime = .now()
    private let ciContext = CIContext()

    var hasViewers: Bool {
        lock.lock(); defer { lock.unlock() }
        return viewers > 0
    }

    /// Called from the SCK sample callback; throttled JPEG conversion (~12 fps max).
    func publish(sampleBuffer: CMSampleBuffer) {
        guard hasViewers else { return }
        let now = DispatchTime.now()
        let dt = Double(now.uptimeNanoseconds - lastPublish.uptimeNanoseconds) / 1e9
        guard dt > 0.08 else { return }
        lastPublish = now

        guard let pb = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let ci = CIImage(cvPixelBuffer: pb)
        guard let cg = ciContext.createCGImage(ci, from: ci.extent) else { return }
        let data = UIImage(cgImage: cg).jpegData(compressionQuality: 0.5)
        lock.lock(); latestJPEG = data; lock.unlock()
    }

    private func current() -> Data? {
        lock.lock(); defer { lock.unlock() }
        return latestJPEG
    }

    /// Takes over an accepted NWConnection and streams multipart JPEG forever.
    func serve(_ conn: NWConnection) {
        lock.lock(); viewers += 1; lock.unlock()
        print("[ius] browser viewer connected")

        conn.stateUpdateHandler = { [weak self] state in
            switch state {
            case .cancelled, .failed:
                self?.lock.lock(); self?.viewers -= 1; self?.lock.unlock()
                print("[ius] browser viewer disconnected")
                conn.stateUpdateHandler = nil
            default:
                break
            }
        }

        conn.send(content: Data("HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=iusframe\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n".utf8),
                  completion: .contentProcessed { error in
                      if error != nil { conn.cancel() }
                  })

        DispatchQueue(label: "ius.mjpeg", target: .global()).async { [weak self, weak conn] in
            guard let conn else { return }
            while true {
                let state = conn.state
                if state == .cancelled || state == .failed || state != .ready { break }
                if let d = self?.current() {
                    var payload = Data("--iusframe\r\nContent-Type: image/jpeg\r\nContent-Length: \(d.count)\r\n\r\n".utf8)
                    payload.append(d)
                    payload.append(Data("\r\n".utf8))
                    let sem = DispatchSemaphore(value: 0)
                    conn.send(content: payload, completion: .contentProcessed { _ in
                        sem.signal()
                    })
                    sem.wait()
                }
                Thread.sleep(forTimeInterval: 0.07)
            }
        }
    }
}
