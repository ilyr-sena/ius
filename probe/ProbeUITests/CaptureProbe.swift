import Foundation
import CoreMedia
import CoreVideo

#if canImport(ScreenCaptureKit)
import ScreenCaptureKit

final class CaptureProbe: NSObject, SCStreamDelegate, SCStreamOutput {
    private let q = DispatchQueue(label: "ius.capture")
    private let lock = NSLock()
    private var stream: SCStream?

    private var startedAt: DispatchTime?
    private var firstFrameAt: DispatchTime?
    private var lastFrameAt: DispatchTime?
    private var foregroundFrames = 0
    private var backgroundFrames = 0
    private var backgrounded = false
    private var intervals: [Double] = []
    private var stopError: String?

    func markBackgrounded() {
        lock.lock(); backgrounded = true; lock.unlock()
    }

    func inventory() -> [String: Any] {
        guard #available(iOS 27.0, *) else { return ["available": false] }
        let sem = DispatchSemaphore(value: 0)
        var out: [String: Any] = [:]
        Task.detached {
            do {
                let content = try await SCShareableContent.current
                out["displays"] = content.displays.map {
                    ["width": $0.width, "height": $0.height, "id": Int($0.displayID)] as [String: Any]
                }
                out["appCount"] = content.applications.count
            } catch {
                out["error"] = String(describing: error)
            }
            sem.signal()
        }
        sem.wait()
        return out
    }

    func startCapture(width: Int, height: Int) -> String? {
        guard #available(iOS 27.0, *) else { return "SCK unavailable on this OS" }
        let sem = DispatchSemaphore(value: 0)
        var errOut: String?
        Task.detached { [self] in
            do {
                let content = try await SCShareableContent.current
                guard let display = content.displays.first else {
                    throw NSError(domain: "ius", code: 1,
                                  userInfo: [NSLocalizedDescriptionKey: "no displays in shareable content"])
                }
                let filter = SCContentFilter(display: display,
                                             excludingApplications: [],
                                             exceptingWindows: [])
                let cfg = SCStreamConfiguration()
                cfg.width = width
                cfg.height = height
                cfg.minimumFrameInterval = CMTime(value: 1, timescale: 60)
                cfg.queueDepth = 3
                cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                let s = SCStream(filter: filter, configuration: cfg, delegate: self)
                try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: q)
                lock.lock(); startedAt = DispatchTime.now(); lock.unlock()
                try await s.startCapture()
                lock.lock(); stream = s; lock.unlock()
            } catch {
                errOut = String(describing: error)
            }
            sem.signal()
        }
        sem.wait()
        return errOut
    }

    func stopCapture() {
        guard #available(iOS 27.0, *) else { return }
        lock.lock(); let s = stream; stream = nil; lock.unlock()
        let sem = DispatchSemaphore(value: 0)
        Task.detached {
            if let s { try? await s.stopCapture() }
            sem.signal()
        }
        _ = sem.wait(timeout: .now() + 5)
    }

    func stats() -> [String: Any] {
        lock.lock(); defer { lock.unlock() }
        var out: [String: Any] = [
            "totalFrames": foregroundFrames + backgroundFrames,
            "foregroundFrames": foregroundFrames,
            "backgroundFrames": backgroundFrames,
        ]
        if let s = startedAt, let f = firstFrameAt {
            out["firstFrameMs"] = Double(f.uptimeNanoseconds - s.uptimeNanoseconds) / 1e6
        }
        if !intervals.isEmpty {
            let sorted = intervals.sorted()
            out["intervalMsP50"] = sorted[sorted.count / 2]
            out["intervalMsP95"] = sorted[Int(Double(sorted.count - 1) * 0.95)]
            out["intervalMsMax"] = sorted.last
        }
        if let e = stopError { out["stopError"] = e }
        return out
    }

    // SCStreamOutput
    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
                of type: SCStreamOutputType) {
        guard type == .screen else { return }
        lock.lock(); defer { lock.unlock() }
        let now = DispatchTime.now()
        if firstFrameAt == nil { firstFrameAt = now }
        if let last = lastFrameAt {
            intervals.append(Double(now.uptimeNanoseconds - last.uptimeNanoseconds) / 1e6)
        }
        lastFrameAt = now
        if backgrounded { backgroundFrames += 1 } else { foregroundFrames += 1 }
    }

    // SCStreamDelegate
    func stream(_ stream: SCStream, didStopWithError error: Error) {
        lock.lock(); stopError = String(describing: error); lock.unlock()
    }
}

#else

final class CaptureProbe: NSObject {
    private let lock = NSLock()

    func markBackgrounded() {}

    func inventory() -> [String: Any] {
        return [
            "available": false,
            "error": "ScreenCaptureKit module absent from this SDK — first ships for iOS 27 (Xcode 27)",
        ]
    }

    func startCapture(width: Int, height: Int) -> String? {
        return "ScreenCaptureKit unavailable: requires iOS 27+ SDK"
    }

    func stopCapture() {}

    func stats() -> [String: Any] {
        return [
            "available": false,
            "totalFrames": 0,
            "foregroundFrames": 0,
            "backgroundFrames": 0,
        ]
    }
}

#endif
