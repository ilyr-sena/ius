import Foundation

/// Relays browser gesture commands (over the probe WebSocket) to the
/// WebDriverAgent server running on this device's loopback interface.
final class WdaRelay {
    static let shared = WdaRelay()

    private let base = URL(string: "http://127.0.0.1:8100")!
    private let lock = NSLock()
    private var sessionId: String?
    private var busy = false

    /// Entry point for messages arriving from a viewer websocket.
    func handle(message: [String: Any]) -> [String: Any] {
        let kind = message["kind"] as? String ?? ""
        switch kind {
        case "actions":
            guard let actions = message["actions"] as? [[String: Any]] else {
                return ["ok": false, "error": "missing actions"]
            }
            let sync = message["wait"] as? Bool ?? true
            if sync { return performActionsSync(actions) }
            performActions(actions) { ok, err in
                _ = ok; _ = err
            }
            return ["ok": true, "queued": true]
        case "ping":
            return ["ok": true]
        default:
            return ["ok": false, "error": "unknown kind"]
        }
    }

    // ---- public-ish sync wrapper used by the relay ------------------------

    private func performActionsSync(_ actions: [[String: Any]]) -> [String: Any] {
        let sem = DispatchSemaphore(value: 0)
        var result: [String: Any] = ["ok": false, "error": "timeout"]
        performActions(actions) { ok, err in
            result = ok ? ["ok": true] : ["ok": false, "error": err ?? "?"]
            sem.signal()
        }
        _ = sem.wait(timeout: .now() + 30)
        return result
    }

    private func performActions(_ actions: [[String: Any]],
                                completion: @escaping (Bool, String?) -> Void) {
        ensureSession { [weak self] sid in
            guard let self, let sid else {
                completion(false, "no WDA session - is the WebDriverAgent runner active?")
                return
            }
            var req = URLRequest(url: self.base.appendingPathComponent("session/\(sid)/actions"))
            req.httpMethod = "POST"
            req.timeoutInterval = 30
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try? JSONSerialization.data(
                withJSONObject: ["actions": actions])
            URLSession.shared.dataTask(with: req) { _, resp, err in
                if let err {
                    completion(false, String(describing: err))
                    return
                }
                let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
                completion((200..<300).contains(code), "http \(code)")
            }.resume()
        }
    }

    private func ensureSession(_ done: @escaping (String?) -> Void) {
        lock.lock()
        if let sid = sessionId {
            lock.unlock()
            done(sid)
            return
        }
        if busy {                          // another create in flight
            lock.unlock()
            DispatchQueue.global().asyncAfter(deadline: .now() + 0.3) { [weak self] in
                self?.ensureSession(done)
            }
            return
        }
        busy = true
        lock.unlock()

        var req = URLRequest(url: base.appendingPathComponent("session"))
        req.httpMethod = "POST"
        req.timeoutInterval = 20
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: ["capabilities": [:]])

        URLSession.shared.dataTask(with: req) { [weak self] data, resp, err in
            defer {
                self?.lock.lock(); self?.busy = false; self?.lock.unlock()
            }
            guard err == nil,
                  let data,
                  let http = resp as? HTTPURLResponse,
                  (200..<300).contains(http.statusCode),
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let value = obj["value"] as? [String: Any],
                  let sid = value["sessionId"] as? String else {
                print("[ius] wda session create failed")
                done(nil)
                return
            }
            self?.lock.lock(); self?.sessionId = sid; self?.lock.unlock()
            print("[ius] wda session: \(sid)")
            done(sid)
        }.resume()
    }
}
