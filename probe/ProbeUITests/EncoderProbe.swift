import Foundation
import VideoToolbox
import CoreMedia
import CoreVideo

final class EncoderProbe {
    final class Ctx {
        let lock = NSLock()
        let done = DispatchSemaphore(value: 0)
        var remaining = 0
        var bytes = 0
        var firstError: OSStatus = noErr
    }

    static func run(width: Int, height: Int, frames: Int = 300, fps: Int = 60) -> [String: Any] {
        var report: [String: Any] = ["width": width, "height": height, "frames": frames]
        let ctx = Ctx()
        ctx.remaining = frames

        let cb: VTCompressionOutputCallback = { refCon, _, status, _, sb in
            guard let refCon else { return }
            let ctx = Unmanaged<Ctx>.fromOpaque(refCon).takeUnretainedValue()
            ctx.lock.lock()
            if status != noErr && ctx.firstError == noErr { ctx.firstError = status }
            if let sb { ctx.bytes += CMSampleBufferGetTotalSampleSize(sb) }
            ctx.remaining -= 1
            let finished = ctx.remaining == 0
            ctx.lock.unlock()
            if finished { ctx.done.signal() }
        }
        let refCon = Unmanaged.passUnretained(ctx).toOpaque()

        var session: VTCompressionSession?
        let hwSpec = [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true] as CFDictionary
        var status = VTCompressionSessionCreate(allocator: nil, width: Int32(width), height: Int32(height),
                                                codecType: kCMVideoCodecType_H264,
                                                encoderSpecification: hwSpec,
                                                imageBufferAttributes: nil,
                                                compressedDataAllocator: nil,
                                                outputCallback: cb, refcon: refCon,
                                                compressionSessionOut: &session)
        report["hw"] = status == noErr ? "required" : "fallback"
        if status != noErr {
            let swSpec = [kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: true] as CFDictionary
            status = VTCompressionSessionCreate(allocator: nil, width: Int32(width), height: Int32(height),
                                                codecType: kCMVideoCodecType_H264,
                                                encoderSpecification: swSpec,
                                                imageBufferAttributes: nil,
                                                compressedDataAllocator: nil,
                                                outputCallback: cb, refcon: refCon,
                                                compressionSessionOut: &session)
        }
        guard status == noErr, let s = session else {
            report["error"] = "VTCompressionSessionCreate status=\(status)"
            return report
        }

        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_RealTime, value: true as CFBoolean)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_AverageBitRate, value: 12_000_000 as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_ExpectedFrameRate, value: 60 as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_MaxKeyFrameInterval, value: 60 as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_ProfileLevel, value: kVTProfileLevel_H264_High_AutoLevel)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_AllowFrameReordering, value: false as CFBoolean)

        var pool: CVPixelBufferPool?
        let pbAttrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
            kCVPixelBufferPoolMinimumBufferCountKey: 4,
        ]
        CVPixelBufferPoolCreate(nil, nil, pbAttrs as CFDictionary, &pool)
        guard let pool else { report["error"] = "pixel buffer pool failed"; return report }

        let t0 = DispatchTime.now()
        var submitted = 0
        for i in 0..<frames {
            var pb: CVPixelBuffer?
            CVPixelBufferPoolCreatePixelBuffer(nil, pool, &pb)
            guard let pb else { break }
            CVPixelBufferLockBaseAddress(pb, [])
            if let base = CVPixelBufferGetBaseAddress(pb) {
                memset(base, Int32(truncatingIfNeeded: i &* 61), CVPixelBufferGetDataSize(pb))
            }
            CVPixelBufferUnlockBaseAddress(pb, [])
            let pts = CMTime(value: CMTimeValue(i), timescale: CMTimeScale(fps))
            VTCompressionSessionEncodeFrame(s, imageBuffer: pb, presentationTimeStamp: pts,
                                            duration: .invalid, frameProperties: nil,
                                            sourceFrameRefcon: nil, infoFlagsOut: nil)
            submitted += 1
        }
        if ctx.done.wait(timeout: .now() + 120) == .timedOut {
            report["error"] = "encode timed out (submitted \(submitted))"
            return report
        }
        let dt = Double(DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds) / 1e9
        report["encodeFps"] = Double(submitted) / dt
        report["msPerFrame"] = dt * 1000.0 / Double(max(submitted, 1))
        report["mbps"] = Double(ctx.bytes) * 8.0 / dt / 1e6
        report["firstError"] = Int(ctx.firstError)
        report["submitted"] = submitted
        return report
    }
}