import SwiftUI

@main
struct ProbeTargetApp: App {
    var body: some Scene {
        WindowGroup {
            ProbeStatusView()
                .onAppear { ProbeOrchestrator.shared.start(port: 9100) }
        }
    }
}

struct ProbeStatusView: View {
    @State private var phase = ProbeOrchestrator.shared.currentPhase()
    private let timer = Timer.publish(every: 0.5, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(spacing: 16) {
            Text("IUS ScreenCaptureKit probe")
                .font(.headline)
            Text("phase: \(phase)")
                .font(.subheadline.monospaced())
            Text("Streams run perpetually once you accept the picker.\nPC: python3 tools/stream_run.py --wda\nH.264+control: /stream.html (needs WDA running)")
                .font(.footnote)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding()
        .onReceive(timer) { _ in phase = ProbeOrchestrator.shared.currentPhase() }
    }
}
