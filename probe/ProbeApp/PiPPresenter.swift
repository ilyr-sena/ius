import AVFoundation
import AVKit
import UIKit

enum ClipFactory {
    static func makeLoopClip(url: URL, width: Int = 320, height: Int = 180,
                             fps: Int32 = 30, seconds: Double = 4) throws {
        let writer = try AVAssetWriter(outputURL: url, fileType: .mp4)
        let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
            AVVideoCodecKey: AVVideoCodecType.h264,
            AVVideoWidthKey: width,
            AVVideoHeightKey: height,
        ])
        let attrs: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey as String: width,
            kCVPixelBufferHeightKey as String: height,
        ]
        let adaptor = AVAssetWriterInputPixelBufferAdaptor(assetWriterInput: input,
                                                           sourcePixelBufferAttributes: attrs)
        writer.add(input)
        writer.startWriting()
        writer.startSession(atSourceTime: .zero)

        let total = Int(seconds * Double(fps))
        for i in 0..<total {
            while !input.isReadyForMoreMediaData { Thread.sleep(forTimeInterval: 0.005) }
            var pb: CVPixelBuffer?
            CVPixelBufferCreate(kCFAllocatorDefault, width, height,
                                kCVPixelFormatType_32BGRA, attrs as CFDictionary, &pb)
            guard let pb else { continue }
            CVPixelBufferLockBaseAddress(pb, [])
            if let base = CVPixelBufferGetBaseAddress(pb) {
                let stride = CVPixelBufferGetBytesPerRow(pb)
                let px = base.assumingMemoryBound(to: UInt8.self)
                let t2pi = Float(i) / Float(total) * Float.pi * 2
                for row in 0..<height {
                    let gBase = Float(row) / 25.0
                    let rowOff = row * stride
                    for col in 0..<width {
                        let s1 = sin(t2pi + Float(col) / 25.0)
                        let s2 = sin(t2pi + gBase)
                        let o = rowOff + col * 4
                        px[o] = UInt8(127 + 127 * s1)
                        px[o + 1] = UInt8(127 + 127 * s2)
                        px[o + 2] = UInt8(truncatingIfNeeded: col + i)
                        px[o + 3] = 255
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(pb, [])
            let pts = CMTime(value: CMTimeValue(i), timescale: fps)
            adaptor.append(pb, withPresentationTime: pts)
        }
        input.markAsFinished()
        let sem = DispatchSemaphore(value: 0)
        writer.finishWriting { sem.signal() }
        sem.wait()
        guard writer.status == .completed else {
            throw NSError(domain: "ius", code: 10,
                          userInfo: [NSLocalizedDescriptionKey: "clip writer failed: \(String(describing: writer.error))"])
        }
    }
}

final class PiPPresenter: NSObject {
    static let shared = PiPPresenter()

    private var player: AVQueuePlayer?
    private var looper: AVPlayerLooper?
    private var pip: AVPictureInPictureController?
    private(set) var lastError: String?

    /// Starts inline looping playback with automatic PiP on leave-foreground.
    /// Safe to call repeatedly; only acts once.
    func ensureStarted() -> Bool {
        if player != nil { return true }
        do {
            let url = FileManager.default.temporaryDirectory.appendingPathComponent("ius-loop.mp4")
            if !FileManager.default.fileExists(atPath: url.path) {
                try ClipFactory.makeLoopClip(url: url)
            }
            let item = AVPlayerItem(url: url)
            let p = AVQueuePlayer()
            looper = AVPlayerLooper(player: p, templateItem: item)

            try AVAudioSession.sharedInstance().setCategory(.playback)
            try AVAudioSession.sharedInstance().setActive(true)

            DispatchQueue.main.sync {
                let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                guard let root = scenes.first?.windows.first(where: { $0.isKeyWindow })?.rootViewController?.view else {
                    self.lastError = "no key window"
                    return
                }
                let l = AVPlayerLayer(player: p)
                l.frame = CGRect(x: 12, y: 90, width: 140, height: 79)
                l.backgroundColor = UIColor.black.cgColor
                root.layer.addSublayer(l)
                let pc = AVPictureInPictureController(playerLayer: l)
                pc?.canStartPictureInPictureAutomaticallyFromInline = true
                self.pip = pc
                p.play()
                self.player = p
            }
            return player != nil
        } catch {
            lastError = String(describing: error)
            return false
        }
    }

    func isActive() -> Bool { player != nil && player?.timeControlStatus != .stopped }
}
