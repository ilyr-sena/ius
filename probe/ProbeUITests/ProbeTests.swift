import XCTest

final class ProbeTests: XCTestCase {
    func testProbeRunner() {
        ProbeOrchestrator.shared.start(port: 9100)
        print("[ius] probe server listening on :9100")
        RunLoop.main.run()   // never returns — same pattern WDA uses
    }
}