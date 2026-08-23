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
    private var grantedFilter: SCContentFilter?

    private var startedAt: DispatchTime?
    private var firstFrameAt: DispatchTime?
    private var lastFrameAt: DispatchTime?
    private var foregroundFrames = 0
    private var backgroundFrames = 0
    private var lockedFrames = 0
    private var backgrounded = false
    private var locked = false
    private var intervals: [Double] = []
    private var stopError: String?

    private let filterSem = DispatchSemaphore(value: 0)
    private var pendingFilter: SCContentFilter?
    private var pickerError: String?

    func markBackgrounded() {
        lock.lock(); backgrounded = true; lock.unlock()
    }

    func setLocked(_ l: Bool) {
        lock.lock(); let changed = locked != l; locked = l; lock.unlock()
        if changed { print("[ius] device lock state -> \(l)") }
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

                lock.lock(); pendingFilter = nil; pickerError = nil; lock.unlock()
                var presented = false
                DispatchQueue.main.sync {
                    picker.add(self)
                    picker.isActive = true
                    picker.present()
                    presented = picker.isActive
                }
                print("[ius] picker.present() called (isActive=\(presented)) — select the full display (timeout \(Self.pickerTimeoutSeconds)s)")
                if !presented {
                    throw NSError(domain: "ius", code: 5,
                                  userInfo: [NSLocalizedDescriptionKey: "SCContentSharingPicker did not become active — check NSScreenCaptureUsageDescription/screen-recording permission"])
                }

                if filterSem.wait(timeout: .now() + .init(Self.pickerTimeoutSeconds)) == .timedOut {
                    throw NSError(domain: "ius", code: 2,
                                  userInfo: [NSLocalizedDescriptionKey: "timed out waiting for picker selection — did the system picker appear and get confirmed on screen?"])
                }
                lock.lock(); let err = pickerError; let filter = pendingFilter; lock.unlock()
                if let err { throw NSError(domain: "ius", code: 3,
                                           userInfo: [NSLocalizedDescriptionKey: "picker failed: \(err)"]) }
                guard let filter else {
                    throw NSError(domain: "ius", code: 4,
                                  userInfo: [NSLocalizedDescriptionKey: "no filter returned from picker"])
                }

                lock.lock(); grantedFilter = filter; lock.unlock()
                errOut = startStream(filter: filter, width: width, height: height)
            } catch {
                errOut = String(describing: error)
            }
            sem.signal()
        }
        sem.wait()
        return errOut
    }

    /// Rebuilds the stream from the previously-granted filter — no picker interaction.
    func restart(width: Int, height: Int) -> String? {
        lock.lock(); let f = grantedFilter; stopError = nil; lock.unlock()
        guard let f else { return "no cached filter" }
        return startStream(filter: f, width: width, height: height)
    }

    func hasCachedFilter() -> Bool {
        lock.lock(); defer { lock.unlock() }
        return grantedFilter != nil
    }

    private func startStream(filter: SCContentFilter, width: Int, height: Int) -> String? {
        stopStreamOnly()
        let sem = DispatchSemaphore(value: 0)
        var errOut: String?
        Task.detached { [self] in
            do {
                let cfg = SCStreamConfiguration()
                cfg.width = width
                cfg.height = height

                let s = SCStream(filter: filter, configuration: cfg, delegate: self)
                try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: q)
                lock.lock();
                startedAt = DispatchTime.now()
                firstFrameAt = nil
                lastFrameAt = nil
                lock.unlock()
                try await s.startCapture()
                lock.lock(); stream = s; lock.unlock()
                print("[ius] capture stream running")
            } catch {
                errOut = String(describing: error)
            }
            sem.signal()
        }
        sem.wait()
        return errOut
    }

    func stopStreamOnly() {
        lock.lock(); let s = stream; stream = nil; lock.unlock()
        guard let s else { return }
        let sem = DispatchSemaphore(value: 0)
        Task.detached { [self] in
            try? await s.stopCapture()
            sem.signal()
        }
        _ = sem.wait(timeout: .now() + 5)
    }

    func stopCapture() {
        stopStreamOnly()
        Task.detached { [self] in
            SCContentSharingPicker.shared.remove(self)
        }
    }

    func stats() -> [String: Any] {
        lock.lock(); defer { lock.unlock() }
        var out: [String: Any] = [
            "totalFrames": foregroundFrames + backgroundFrames + lockedFrames,
            "foregroundFrames": foregroundFrames,
            "backgroundFrames": backgroundFrames,
            "lockedFrames": lockedFrames,
        ]
        if let s = startedAt, let f = firstFrameAt {
            out["firstFrameMs"] = Double(f.uptimeNanoseconds - s.uptimeNanoseconds) / 1e6
        }
        if let last = lastFrameAt {
            out["msSinceLastFrame"] = Double(DispatchTime.now().uptimeNanoseconds - last.uptimeNanoseconds) / 1e6
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
        if locked { lockedFrames += 1 }
        else if backgrounded { backgroundFrames += 1 }
        else { foregroundFrames += 1 }
    }

    // SCStreamDelegate
    func stream(_ stream: SCStream, didStopWithError error: Error) {
        lock.lock()
        if stopError == nil { stopError = String(describing: error) }
        lock.unlock()
        print("[ius] stream stopped: \(error)")
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
    func setLocked(_ l: Bool) {}

    func inventory() -> [String: Any] {
        return [
            "available": false,
            "error": "ScreenCaptureKit module absent from this SDK — first ships for iOS 27 (Xcode 27)",
        ]
    }

    func startCapture(width: Int, height: Int) -> String? {
        return "ScreenCaptureKit unavailable: requires iOS 27+ SDK"
    }

    func restart(width: Int, height: Int) -> String? { "unavailable" }
    func hasCachedFilter() -> Bool { false }
    func stopCapture() {}

    func stats() -> [String: Any] {
        return [
            "available": false,
            "totalFrames": 0,
            "foregroundFrames": 0,
            "backgroundFrames": 0,
            "lockedFrames": 0,
        ]
    }
}

#endif
