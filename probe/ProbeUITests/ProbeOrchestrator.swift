import Foundation
import UIKit
import XCTest

final class ProbeOrchestrator {
    static let shared = ProbeOrchestrator()

    private let server = TinyHTTPServer()
    private let capture = CaptureProbe()
    private let lock = NSLock()
    private var phase = "idle"
    private var report: [String: Any] = [:]
    private var running = false
    private var backgrounded = false

    func start(port: UInt16) {
        server.handler = { [weak self] method, path in
            guard let self else { return (503, ["error": "gone"]) }
            return self.route(method: method, path: path)
        }
        server.start(port: port)
        NotificationCenter.default.addObserver(
            forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: nil
        ) { [weak self] _ in
            guard let self else { return }
            self.lock.lock()
            self.backgrounded = true
            self.lock.unlock()
            self.capture.markBackgrounded()
            print("[ius] didEnterBackground observed")
        }
    }

    private func setPhase(_ p: String) {
        lock.lock(); phase = p; lock.unlock()
        print("[ius] phase: \(p)")
    }

    private func route(method: String, path: String) -> (Int, [String: Any]) {
        switch (method, path) {
        case ("GET", "/status"):
            lock.lock(); let p = phase; lock.unlock()
            return (200, ["phase": p])
        case ("POST", "/probe/start"):
            lock.lock()
            if running { lock.unlock(); return (409, ["error": "already running"]) }
            running = true
            lock.unlock()
            DispatchQueue.global(qos: .utility).async { [self] in runPlan() }
            return (200, ["started": true])
        case ("GET", "/probe/report"):
            lock.lock(); var out = report; out["phase"] = phase; lock.unlock()
            return (200, out)
        default:
            return (404, ["error": "not found"])
        }
    }

    private func runPlan() {
        setPhase("inventory")
        let inv = capture.inventory()
        lock.lock(); report["inventory"] = inv; lock.unlock()

        guard let displays = inv["displays"] as? [[String: Any]], let d = displays.first else {
            lock.lock(); report["verdict"] = "RED: no displays via SCK: \(inv["error"] ?? "?")"; lock.unlock()
            setPhase("done")
            lock.lock(); running = false; lock.unlock()
            return
        }
        let w = d["width"] as? Int ?? 1170
        let h = d["height"] as? Int ?? 2532

        setPhase("start-capture")
        if let err = capture.startCapture(width: w, height: h) {
            lock.lock()
            report["captureStartError"] = err
            report["verdict"] = "RED: direct capture failed — \(err)"
            lock.unlock()
            setPhase("done")
            lock.lock(); running = false; lock.unlock()
            return
        }
        Thread.sleep(forTimeInterval: 1)

        setPhase("foreground-fps")
        let f0 = capture.stats()["totalFrames"] as? Int ?? 0
        Thread.sleep(forTimeInterval: 8)
        let fgFps = Double((capture.stats()["totalFrames"] as? Int ?? 0) - f0) / 8.0
        lock.lock(); report["foregroundFps"] = fgFps; lock.unlock()

        setPhase("awaiting-background")
        DispatchQueue.main.async { [self] in
            XCUIDevice.shared.press(.home)
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [self] in
                lock.lock(); let bg = backgrounded; lock.unlock()
                if !bg { XCUIApplication(bundleIdentifier: "com.apple.Preferences").launch() }
            }
        }
        var bg = false
        for _ in 0..<30 {
            Thread.sleep(forTimeInterval: 0.5)
            lock.lock(); bg = backgrounded; lock.unlock()
            if bg { break }
        }
        lock.lock(); report["backgrounded"] = bg; lock.unlock()

        if bg {
            setPhase("background-fps")
            let b0 = capture.stats()["backgroundFrames"] as? Int ?? 0
            Thread.sleep(forTimeInterval: 10)
            let st = capture.stats()
            let bFps = Double((st["backgroundFrames"] as? Int ?? 0) - b0) / 10.0
            lock.lock()
            report["backgroundFps"] = bFps
            if let e = st["stopError"] as? String { report["captureStopError"] = e }
            lock.unlock()
        }

        capture.stopCapture()
        Thread.sleep(forTimeInterval: 0.5)
        lock.lock(); report["captureStats"] = capture.stats(); lock.unlock()

        setPhase("encoder")
        lock.lock(); report["encoder"] = EncoderProbe.run(width: w, height: h); lock.unlock()

        lock.lock()
        let verdict: String
        if !bg {
            verdict = "YELLOW: never backgrounded — rerun, swipe up when told"
        } else if (report["backgroundFps"] as? Double ?? 0) >= 40 {
            verdict = "GREEN: direct full-screen capture survives background at full rate"
        } else if fgFps >= 40 {
            verdict = "YELLOW: capture OK foreground, throttled/stopped in background"
        } else {
            verdict = "RED: capture rate too low even in foreground"
        }
        report["verdict"] = verdict
        lock.unlock()
        setPhase("done")
        lock.lock(); running = false; lock.unlock()
    }
}