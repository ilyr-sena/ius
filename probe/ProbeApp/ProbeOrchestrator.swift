import Foundation
import UIKit

final class ProbeOrchestrator {
    static let shared = ProbeOrchestrator()

    private let server = TinyHTTPServer()
    private let capture = CaptureProbe()
    private let lock = NSLock()
    private var phase = "idle"
    private var report: [String: Any] = [:]
    private var running = false
    private var backgrounded = false
    private var deviceLocked = false
    private var started = false

    func currentPhase() -> String {
        lock.lock(); defer { lock.unlock() }
        return phase
    }

    func start(port: UInt16) {
        guard !started else { return }
        started = true
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
        NotificationCenter.default.addObserver(
            forName: UIApplication.protectedDataWillBecomeUnavailableNotification, object: nil, queue: nil
        ) { [weak self] _ in
            guard let self else { return }
            self.lock.lock(); self.deviceLocked = true; self.lock.unlock()
            self.capture.setLocked(true)
        }
        NotificationCenter.default.addObserver(
            forName: UIApplication.protectedDataDidBecomeAvailableNotification, object: nil, queue: nil
        ) { [weak self] _ in
            guard let self else { return }
            self.lock.lock(); self.deviceLocked = false; self.lock.unlock()
            self.capture.setLocked(false)
        }
    }

    private func setPhase(_ p: String) {
        lock.lock(); phase = p; lock.unlock()
        print("[ius] phase: \(p)")
    }

    private func route(method: String, path: String) -> (Int, [String: Any]) {
        switch (method, path) {
        case ("GET", "/status"):
            return (200, ["phase": currentPhase()])
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
        setPhase("audio")
        let audioOK = AudioKeepAlive.shared.start()
        lock.lock()
        report["audio"] = audioOK ? "running" : "failed: \(AudioKeepAlive.shared.lastError ?? "?")"
        lock.unlock()
        print("[ius] audio keep-alive \(audioOK ? "running" : "FAILED")")

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
        print("[ius] a system screen-sharing picker will appear — select the full display")
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
        print("[ius] SWIPE UP to the home screen now (keep device unlocked, screen on)")
        var bg = false
        for _ in 0..<60 {
            Thread.sleep(forTimeInterval: 0.5)
            lock.lock(); bg = backgrounded; lock.unlock()
            if bg { break }
        }
        lock.lock(); report["backgrounded"] = bg; lock.unlock()

        if bg {
            setPhase("background-fps")
            let b0 = capture.stats()["totalFrames"] as? Int ?? 0
            Thread.sleep(forTimeInterval: 10)
            let st = capture.stats()
            let bFps = Double((st["backgroundFrames"] as? Int ?? 0) - b0) / 10.0
            lock.lock()
            report["backgroundFps"] = bFps
            if let e = st["stopError"] as? String { report["captureStopError"] = e }
            lock.unlock()
        }

        // ---- Lock survival test (timed; works with or without passcode) ----
        setPhase("awaiting-lock")
        print("[ius] LOCK TEST: press the LOCK BUTTON within 3s, keep locked ~10s, unlock when told")
        Thread.sleep(forTimeInterval: 3)            // window for the user to press lock
        lock.lock(); let lockEventSeen = deviceLocked; lock.unlock()
        report["lockEventDetected"] = lockEventSeen

        let preLockTotal = capture.stats()["totalFrames"] as? Int ?? 0
        let lockStart = DispatchTime.now()
        for _ in 0..<24 {                           // fixed ~12s locked window
            Thread.sleep(forTimeInterval: 0.5)
        }
        let lockWindowS = Double(DispatchTime.now().uptimeNanoseconds - lockStart.uptimeNanoseconds) / 1e9
        let st1 = capture.stats()
        let lockedDelta = (st1["totalFrames"] as? Int ?? 0) - preLockTotal
        lock.lock(); report["lockWindowS"] = lockWindowS; lock.unlock()
        report["framesWhileLocked"] = lockedDelta
        report["lockedFps"] = Double(lockedDelta) / max(lockWindowS, 0.001)
        if let e = st1["stopError"] as? String { report["streamStoppedOnLock"] = e }

        setPhase("awaiting-unlock")
        print("[ius] UNLOCK THE PHONE NOW")
        let r0 = DispatchTime.now()
        var cur = capture.stats()
        if (cur["msSinceLastFrame"] as? Double ?? 9_999) > 2_000 || cur["stopError"] != nil {
            print("[ius] stream dead after lock — restarting from cached filter")
            if let err = capture.restart(width: w, height: h) {
                report["restartError"] = err
                print("[ius] restart FAILED: \(err)")
            } else {
                report["autoRestarted"] = true
            }
        }
        let target = (cur["totalFrames"] as? Int ?? 0) + 1
        var recMs: Double = -1
        for _ in 0..<60 {                           // up to 15s for frames to flow again
            Thread.sleep(forTimeInterval: 0.25)
            cur = capture.stats()
            if (cur["totalFrames"] as? Int ?? 0) >= target,
               (cur["msSinceLastFrame"] as? Double ?? 9_999) < 1_000 {
                recMs = Double(DispatchTime.now().uptimeNanoseconds - r0.uptimeNanoseconds) / 1e6
                break
            }
        }
        report["recoveryMs"] = recMs
        let p0 = capture.stats()["totalFrames"] as? Int ?? 0
        setPhase("post-unlock-fps")
        Thread.sleep(forTimeInterval: 5)
        let p1 = capture.stats()["totalFrames"] as? Int ?? 0
        report["postUnlockFps"] = Double(p1 - p0) / 5.0
        // ---- end lock test ----

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
