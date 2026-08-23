import AVFoundation

/// Keeps the process at elevated execution priority in the background by running
/// a genuine audio I/O graph (playback category + looping near-silent tone).
final class AudioKeepAlive {
    static let shared = AudioKeepAlive()

    private var engine: AVAudioEngine?
    private var player: AVAudioPlayerNode?
    private(set) var lastError: String?

    func start() -> Bool {
        guard engine == nil else { return true }
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, mode: .default)
            try session.setActive(true)

            let e = AVAudioEngine()
            let p = AVAudioPlayerNode()
            e.attach(p)
            let format = AVAudioFormat(standardFormatWithSampleRate: 44_100, channels: 1)!
            e.connect(p, to: e.mainMixerNode, format: format)

            let frames = AVAudioFrameCount(44_100)
            guard let buf = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames) else {
                throw NSError(domain: "ius", code: 20,
                              userInfo: [NSLocalizedDescriptionKey: "pcm buffer alloc failed"])
            }
            buf.frameLength = frames
            let ch = buf.floatChannelData![0]
            for i in 0..<Int(frames) {
                ch[i] = 0.005 * sinf(Float(i) * 2 * Float.pi * 220 / 44_100)
            }

            p.scheduleBuffer(buf, at: nil, options: [.loops])
            try e.start()
            p.play()

            engine = e
            player = p
            return true
        } catch {
            lastError = String(describing: error)
            return false
        }
    }

    func stop() {
        player?.stop()
        engine?.stop()
        engine = nil
        player = nil
    }
}
