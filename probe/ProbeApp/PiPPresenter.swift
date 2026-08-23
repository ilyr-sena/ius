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
                let t = Float(i) / Float(total)
                for row in 0..<height {
                    let line = base.advanced(by: row * stride)
                    for col in 0..<width {
                        let p = line.advanced(by: col * 4)
                        p.storeBytes(order: .littleEndian, as: UInt8.self,
                                     value: UInt8(127 + 127 * sin(t * .pi * 2 + Float(col) / 25)))
                        p.advanced(by: 1).storeBytes(order: .littleEndian, as: UInt8.self,
                                                     value: UInt8(127 + 127 * sin(t * .pi * 2 + Float(row) / 25)))
                        p.advanced(by: 2).storeBytes(order: .littleEndian, as: UInt8.self,
                                                     value: UInt8(truncatingIfNeeded: Int(Float(col) + t * 255)))
                        p.advanced(by: 3).storeBytes(order: .littleEndian, as: UInt8.self, value: 255)
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
                guard let root = scenes.first?.windows.first(where: \.isKey)?.rootViewController?.view else {
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

    func isActive() -> Bool { player != nil && !(player?.timeControlStatus == .stopped) }
}
