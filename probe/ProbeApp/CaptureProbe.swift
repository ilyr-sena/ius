import Foundation
import UIKit
import CoreMedia
import CoreVideo

#if canImport(ScreenCaptureKit)
import ScreenCaptureKit

final class CaptureProbe: NSObject, SCStreamDelegate, SCStreamOutput, SCContentSharingPickerObserver {
    static let pickerTimeoutSeconds: Int = 120

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

    private let filterSem = DispatchSemaphore(value: 0)
    private var pendingFilter: SCContentFilter?
    private var pickerError: String?

    func markBackgrounded() {
        lock.lock(); backgrounded = true; lock.unlock()
    }

    func inventory() -> [String: Any] {
        let b = UIScreen.main.nativeBounds
        return [
            "available": true,
            "displays": [["width": Int(b.width), "height": Int(b.height), "id": 1]] as [[String: Any]],
            "note": "iOS 27: filter is obtained via SCContentSharingPicker (no shareable-content enumeration)",
        ]
    }

    func startCapture(width: Int, height: Int) -> String? {
        let sem = DispatchSemaphore(value: 0)
        var errOut: String?
        Task.detached { [self] in
            do {
                let picker = SCContentSharingPicker.shared
                let pcfg = SCContentSharingPickerConfiguration()
                picker.defaultConfiguration = pcfg
                picker.add(self)

                lock.lock(); pendingFilter = nil; pickerError = nil; lock.unlock()
                DispatchQueue.main.sync { picker.present() }
                print("[ius] screen-sharing picker presented — select the full display (timeout \(Self.pickerTimeoutSeconds)s)")

                if filterSem.wait(timeout: .now() + .init(Self.pickerTimeoutSeconds)) == .timedOut {
                    throw NSError(domain: "ius", code: 2,
                                  userInfo: [NSLocalizedDescriptionKey: "timed out waiting for picker selection"])
                }
                lock.lock(); let err = pickerError; let filter = pendingFilter; lock.unlock()
                if let err { throw NSError(domain: "ius", code: 3,
                                           userInfo: [NSLocalizedDescriptionKey: "picker failed: \(err)"]) }
                guard let filter else {
                    throw NSError(domain: "ius", code: 4,
                                  userInfo: [NSLocalizedDescriptionKey: "no filter returned from picker"])
                }

                let cfg = SCStreamConfiguration()
                cfg.width = width
                cfg.height = height

                let s = SCStream(filter: filter, configuration: cfg, delegate: self)
                try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: q)
                lock.lock(); startedAt = DispatchTime.now(); lock.unlock()
                try await s.startCapture()
                lock.lock(); stream = s; lock.unlock()
                print("[ius] capture started")
            } catch {
                errOut = String(describing: error)
            }
            sem.signal()
        }
        sem.wait()
        return errOut
    }

    func stopCapture() {
        lock.lock(); let s = stream; stream = nil; lock.unlock()
        let sem = DispatchSemaphore(value: 0)
        Task.detached { [self] in
            if let s { try? await s.stopCapture() }
            SCContentSharingPicker.shared.remove(self)
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

    // SCContentSharingPickerObserver
    func contentSharingPicker(_ picker: SCContentSharingPicker, didUpdateWith newFilter: SCContentFilter,
                              for stream: SCStream?) {
        lock.lock(); pendingFilter = newFilter; lock.unlock()
        filterSem.signal()
    }

    func contentSharingPicker(_ picker: SCContentSharingPicker, didCancelFor stream: SCStream?) {
        lock.lock(); pickerError = "cancelled by user"; lock.unlock()
        filterSem.signal()
    }

    func contentSharingPickerStartDidFailWithError(_ error: Error) {
        lock.lock(); pickerError = String(describing: error); lock.unlock()
        filterSem.signal()
    }
}

#else

final class CaptureProbe: NSObject {
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
