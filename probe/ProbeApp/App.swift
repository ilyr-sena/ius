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
            Text("Drive over USB:\npython3 tools/probe_run.py\n\nAn audio keep-alive tone runs during the probe. Swipe home at awaiting-background, keep the screen on, don't lock.")
                .font(.footnote)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding()
        .onReceive(timer) { _ in phase = ProbeOrchestrator.shared.currentPhase() }
    }
}
