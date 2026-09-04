import AVFoundation
import CoreImage
import CoreMedia
import CryptoKit
import Network
import VideoToolbox

// ---------------------------------------------------------------------------
// fMP4 box builders (H.264/AVCC, single video track).
// Every fragment carries mfhd + tfhd + tfdt + trun + mdat — tfdt is what naive
// implementations miss and Safari/MSE need it for accurate timeline mapping.
// ---------------------------------------------------------------------------
private enum MP4 {
    static func u16(_ v: UInt16) -> Data { withUnsafeBytes(of: v.bigEndian) { Data($0) } }
    static func u32(_ v: UInt32) -> Data { withUnsafeBytes(of: v.bigEndian) { Data($0) } }
    static func u64(_ v: UInt64) -> Data { withUnsafeBytes(of: v.bigEndian) { Data($0) } }

    static func box(_ type: String, _ payload: Data) -> Data {
        var out = Data()
        out.append(u32(UInt32(payload.count + 8)))
        out.append(Data(type.utf8))
        out.append(payload)
        return out
    }

    static func fullbox(_ type: String, _ version: UInt8, _ flags: UInt32,
                        _ payload: Data) -> Data {
        // ISO 14496-12: version (1 byte) + flags (3 bytes) = 4 bytes header.
        var head = Data([version])
        head.append(Data([
            UInt8((flags >> 16) & 0xFF),
            UInt8((flags >> 8) & 0xFF),
            UInt8(flags & 0xFF)
        ]))
        return box(type, head + payload)
    }

    static let unityMatrix: Data = {
        var d = Data()
        for v: UInt32 in [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
            d.append(u32(v))
        }
        return d
    }()

    static func buildInit(avcC: Data, width: Int, height: Int) -> Data {
        let ftyp = box("ftyp", Data("iso5".utf8) + u32(0)
                              + Data("iso5".utf8) + Data("iso6".utf8) + Data("mp41".utf8))

        var mvhdBody = Data()
        mvhdBody.append(u32(0)); mvhdBody.append(u32(0))
        mvhdBody.append(u32(60000)); mvhdBody.append(u32(0))
        mvhdBody.append(u32(0x00010000))
        mvhdBody.append(u16(0x0100)); mvhdBody.append(u16(0))
        mvhdBody.append(u32(0)); mvhdBody.append(u32(0))
        mvhdBody.append(unityMatrix)
        for _ in 0..<6 { mvhdBody.append(u32(0)) }
        mvhdBody.append(u32(2))
        let mvhd = fullbox("mvhd", 0, 0, mvhdBody)

        var tkhdBody = Data()
        tkhdBody.append(u32(0)); tkhdBody.append(u32(0))
        tkhdBody.append(u32(1))
        tkhdBody.append(u32(0))
        tkhdBody.append(u32(0))
        tkhdBody.append(u32(0)); tkhdBody.append(u32(0))
        tkhdBody.append(u16(0)); tkhdBody.append(u16(0))
        tkhdBody.append(u16(0)); tkhdBody.append(u16(0))
        tkhdBody.append(unityMatrix)
        tkhdBody.append(u32(UInt32(width) << 16))
        tkhdBody.append(u32(UInt32(height) << 16))
        let tkhd = fullbox("tkhd", 0, 0x0000007, tkhdBody)

        let mdhd = fullbox("mdhd", 0, 0,
            u32(0) + u32(0) + u32(60000) + u32(0) + u16(0x55C4) + u16(0))

        let hdlr = fullbox("hdlr", 0, 0,
            u32(0) + Data("vide".utf8) +
            u32(0) + u32(0) + u32(0) +
            Data("ius".utf8) + Data([0]))

        let vmhd = fullbox("vmhd", 0, 1, u16(0) + u16(0) + u16(0) + u16(0))
        let dref = fullbox("dref", 0, 0, u32(1) + fullbox("url ", 0, 1, Data()))
        let stsd = fullbox("stsd", 0, 0,
            u32(1) + avc1Entry(avcC: avcC, width: width, height: height))
        let stbl = box("stbl",
            stsd
            + fullbox("stts", 0, 0, u32(0))
            + fullbox("stsc", 0, 0, u32(0))
            + fullbox("stsz", 0, 0, u32(0) + u32(0))
            + fullbox("stco", 0, 0, u32(0)))

        let minf = box("minf", vmhd + box("dinf", dref) + stbl)
        let mdia = box("mdia", mdhd + hdlr + minf)
        let trak = box("trak", tkhd + mdia)
        let mvex = box("mvex", fullbox("trex", 0, 0,
            u32(1) + u32(1) + u32(0) + u32(0) + u32(0)))

        return ftyp + box("moov", mvhd + trak + mvex)
    }

    private static func avc1Entry(avcC: Data, width: Int, height: Int) -> Data {
        var d = Data(repeating: 0, count: 6)
        d.append(u16(1))
        d.append(u16(0) + u16(0))
        d.append(Data(repeating: 0, count: 12))
        d.append(u16(UInt16(width)))
        d.append(u16(UInt16(height)))
        d.append(u32(0x00480000))
        d.append(u32(0x00480000))
        d.append(u32(0))
        d.append(u16(1))
        d.append(Data(repeating: 0, count: 32))
        d.append(u16(0x0018))
        d.append(contentsOf: [0xFF, 0xFF])
        d.append(box("avcC", avcC))
        return box("avc1", d)
    }

    /// One-sample fragment: moof (mfhd + tfhd + tfdt + trun) + mdat.
    static func buildFragment(seq: UInt32, baseDecodeTicks: UInt64, duration: UInt32,
                              sampleSize: Int, isSync: Bool, payload: Data) -> Data {
        // sample_flags: is_non_sync_sample(16) | sample_depends_on(2: "not I")|(1: "I")
        let flags: UInt32 = isSync ? 0x02000000 : 0x01010000

        var trunPayload = Data()
        trunPayload.append(u32(1))
        trunPayload.append(u32(0xDEADBEEF))          // data_offset placeholder
        trunPayload.append(u32(duration))
        trunPayload.append(u32(UInt32(sampleSize)))
        trunPayload.append(u32(flags))
        let trun = fullbox("trun", 0, 0x000701, trunPayload)

        // tfdt v1 — base media decode time for this fragment (timescale 60000).
        let tfdt = fullbox("tfdt", 1, 0, u64(baseDecodeTicks))
        let tfhd = fullbox("tfhd", 0, 0x020000, u32(1))   // default-base-is-moof
        let mfhd = fullbox("mfhd", 0, 0, u32(seq))
        let traf = box("traf", tfhd + tfdt + trun)
        var moof = box("moof", mfhd + traf)

        let mdat = box("mdat", payload)

        if let idx = moof.range(of: Data([0xDE, 0xAD, 0xBE, 0xEF])) {
            let off = UInt32(moof.count + 8)
            moof.replaceSubrange(idx, with: u32(off))
        }
        return moof + mdat
    }

    static func codecString(avcC: Data) -> String? {
        guard avcC.count >= 6 else { return nil }
        let i = avcC.startIndex
        return String(format: "avc1.%02X%02X%02X",
                      avcC[i + 1], avcC[i + 2], avcC[i + 3])
    }
}

// ---------------------------------------------------------------------------
// Stream configuration — live-adjustable, persisted across runs.
// ---------------------------------------------------------------------------
struct StreamTuning: Codable {
    var bitrateMbps: Double = 6.0      // target average bitrate
    var maxFps: Double = 0             // 0 = uncapped
    var scale: Double = 1.0            // resolution factor (0.25…1)
    var keyframeSeconds: Double = 1.0  // IDR interval

    static let bitsRange = 0.5...12.0
    static let scaleRange = 0.25...1.0
    static let fpsUpperBound = 60.0

    func clamped() -> StreamTuning {
        var t = self
        t.bitrateMbps = min(max(t.bitrateMbps, Self.bitsRange.lowerBound), Self.bitsRange.upperBound)
        t.scale = min(max(t.scale, Self.scaleRange.lowerBound), Self.scaleRange.upperBound)
        t.maxFps = max(0, min(t.maxFps, Self.fpsUpperBound))
        t.keyframeSeconds = max(0.5, min(t.keyframeSeconds, 10))
        return t
    }
}

// ---------------------------------------------------------------------------
// H.264 encoder: VideoToolbox hardware session → fMP4 segments → broadcast.
// ---------------------------------------------------------------------------
final class H264Stream {
    static let shared = H264Stream()

    struct Client {
        let ws: WebSocketConn
        var joined: Bool
    }

    private let lock = NSLock()
    private var clients: [Client] = []

    // Encoder state
    private var session: VTCompressionSession?
    private var encWidth = 0
    private var encHeight = 0
    private var avcC: Data?
    private var initSegment: Data?
    private var seq: UInt32 = 0
    private var lastTicks: Int64?
    private var baseTicks: Int64?
    private var pendingForceKeyFrame = false
    private var isTearingDown = false      // guards against start-during-teardown races

    // Live tuning + stats
    private(set) var tuning = H264Stream.loadTuning()
    private var tuningRevision: UInt64 = 0
    private var shouldRecreate = false     // create new session at next frame
    private var forcedFpsMinGap: Double = 0
    private var lastAcceptedAt: DispatchTime?

    // Throughput stats
    private var bytesWindow = 0
    private var framesWindow = 0
    private var windowStart = DispatchTime.now()

    // ---- tuning ------------------------------------------------------------

    static func loadTuning() -> StreamTuning {
        let d = UserDefaults.standard
        if d.object(forKey: "ius.h264.bitrateMbps") == nil { return StreamTuning() }
        return StreamTuning(
            bitrateMbps: d.double(forKey: "ius.h264.bitrateMbps"),
            maxFps: d.double(forKey: "ius.h264.maxFps"),
            scale: d.double(forKey: "ius.h264.scale"),
            keyframeSeconds: d.double(forKey: "ius.h264.keyframeSeconds")
        )
    }

    private func persist(_ t: StreamTuning) {
        let d = UserDefaults.standard
        d.set(t.bitrateMbps, forKey: "ius.h264.bitrateMbps")
        d.set(t.maxFps, forKey: "ius.h264.maxFps")
        d.set(t.scale, forKey: "ius.h264.scale")
        d.set(t.keyframeSeconds, forKey: "ius.h264.keyframeSeconds")
    }

    /// Live-apply a new tuning. Bitrate applies in-place; fps changes the gate;
    /// scale forces an encoder recreation at the next frame boundary.
    func applyTuning(_ t: StreamTuning) -> StreamTuning {
        let t = t.clamped()
        lock.lock()
        let rebuild = t.scale != tuning.scale
        tuning = t
        tuningRevision &+= 1
        forcedFpsMinGap = t.maxFps > 0 ? 1.0 / t.maxFps : 0
        if rebuild { shouldRecreate = true }
        persist(t)
        let session = self.session
        lock.unlock()

        // VideoToolbox supports on-the-fly ABR retuning without a full session teardown.
        if let s = session, !rebuild {
            VTSessionSetProperty(s, key: kVTCompressionPropertyKey_AverageBitRate,
                                 value: Int(t.bitrateMbps * 1_000_000) as CFNumber)
            VTSessionSetProperty(s, key: kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                                 value: t.keyframeSeconds as CFNumber)
            print("[ius] h264 tuning live: \(t.bitrateMbps)Mbps fps=\(t.maxFps == 0 ? "max" : "\(t.maxFps)")")
        } else if rebuild {
            print("[ius] h264 tuning change requires encoder rebuild at scale=\(t.scale)")
        }
        return t
    }

    func currentTuning() -> StreamTuning {
        lock.lock(); defer { lock.unlock() }
        return tuning
    }

    // ---- client management -------------------------------------------------

    var hasViewers: Bool {
        lock.lock(); defer { lock.unlock() }
        return !clients.isEmpty
    }

    func clientCount() -> Int {
        lock.lock(); defer { lock.unlock() }
        return clients.count
    }

    func stats() -> [String: Any] {
        lock.lock()
        let (rev, w, h) = (tuningRevision, encWidth, encHeight)
        let sess = session != nil
        let (b, f) = (bytesWindow, framesWindow)
        let clients = self.clients.count
        lock.unlock()
        let winElapsed = max(0.001, Double(DispatchTime.now().uptimeNanoseconds - windowStart.uptimeNanoseconds) / 1e9)
        return [
            "encoder": sess ? "running" : "idle",
            "codec": "h264",
            "width": w, "height": h, "tuningRev": rev,
            "mbps": Double(b) * 8.0 / winElapsed / 1e6,
            "fps": Double(f) / winElapsed,
            "clients": clients,
        ]
    }

    // ---- encode path -------------------------------------------------------

    func push(sampleBuffer: CMSampleBuffer) {
        guard hasViewers else { return }
        guard let pb = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let srcW = CVPixelBufferGetWidth(pb)
        let srcH = CVPixelBufferGetHeight(pb)

        lock.lock()
        let tun = tuning
        let recreate = shouldRecreate || session == nil
        if recreate { shouldRecreate = false }
        let fpsGap = forcedFpsMinGap
        let last = lastAcceptedAt
        lock.unlock()

        // Frame-rate gate: drop frames when we're ahead of the cap.
        if fpsGap > 0, let last {
            let dt = Double(DispatchTime.now().uptimeNanoseconds - last.uptimeNanoseconds) / 1e9
            if dt < fpsGap { return }
        }

        // Scale to the target size on-GPU — encoder gets the correct dims.
        let targetW = Int((Double(srcW) * tun.scale / 2).rounded()) * 2
        let targetH = Int((Double(srcH) * tun.scale / 2).rounded()) * 2
        let (useW, useH) = (max(2, targetW), max(2, targetH))

        if recreate {
            teardownSession()
            ensureSession(width: useW, height: useH, tuning: tun)
        }
        guard let s = session else { return }

        var imageBuffer: CVPixelBuffer = pb
        var toRelease: CVPixelBuffer?
        if useW != srcW || useH != srcH {
            guard let scaled = GPUScaler.shared.scale(pb, to: useW, useH) else { return }
            imageBuffer = scaled
            toRelease = scaled
        }

        let pts = CMSampleBufferGetOutputPresentationTimeStamp(sampleBuffer)
        var props: CFDictionary?
        lock.lock()
        let forceKey = pendingForceKeyFrame
        if forceKey { pendingForceKeyFrame = false }
        lock.unlock()
        if forceKey { props = [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary }

        let err = VTCompressionSessionEncodeFrame(s, imageBuffer: imageBuffer,
                                                  presentationTimeStamp: pts,
                                                  duration: .invalid,
                                                  frameProperties: props,
                                                  sourceFrameRefcon: nil,
                                                  infoFlagsOut: nil)
        if let r = toRelease { _ = r }  // keep alive through encode submission
        if err == noErr {
            lock.lock()
            framesWindow += 1
            lastAcceptedAt = DispatchTime.now()
            lock.unlock()
        }
    }

    private func ensureSession(width: Int, height: Int, tuning: StreamTuning) {
        lock.lock()
        if session != nil || isTearingDown {
            lock.unlock()
            return
        }
        isTearingDown = true
        lock.unlock()

        var session: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            width: Int32(width), height: Int32(height),
            codecType: kCMVideoCodecType_H264,
            encoderSpecification: [
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
            ] as CFDictionary,
            imageBufferAttributes: nil,
            compressedDataAllocator: nil,
            outputCallback: { refCon, _, status, _, sb in
                guard status == noErr, let sb, let refCon else { return }
                let me = Unmanaged<H264Stream>.fromOpaque(refCon).takeUnretainedValue()
                me.handleEncoded(sampleBuffer: sb)
            },
            refcon: Unmanaged.passUnretained(self).toOpaque(),
            compressionSessionOut: &session)

        guard status == noErr, let s = session else {
            print("[ius] h264 encoder create failed: \(status)")
            lock.lock(); isTearingDown = false; lock.unlock()
            return
        }
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_MaxFrameDelayCount, value: 0 as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, value: kCFBooleanTrue)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_AverageBitRate,
                             value: Int(tuning.bitrateMbps * 1_000_000) as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_ExpectedFrameRate,
                             value: 60 as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_MaxKeyFrameInterval,
                             value: Int(60.0 * tuning.keyframeSeconds) as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                             value: tuning.keyframeSeconds as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_AllowFrameReordering,
                             value: kCFBooleanFalse)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_ProfileLevel,
                             value: kVTProfileLevel_H264_High_AutoLevel)

        lock.lock()
        self.session = s
        encWidth = width
        encHeight = height
        avcC = nil
        initSegment = nil
        lastTicks = nil
        baseTicks = nil
        seq = 0
        isTearingDown = false
        lock.unlock()
        print("[ius] hw h264 encoder running \(width)x\(height) @ \(tuning.bitrateMbps)Mbps")
    }

    /// Full, safe teardown: complete outstanding encodes, invalidate, release.
    private func teardownSession() {
        lock.lock()
        guard !isTearingDown, let s = session else { lock.unlock(); return }
        session = nil
        isTearingDown = true
        lock.unlock()

        DispatchQueue(label: "ius.h264.teardown").async { [weak self] in
            VTCompressionSessionCompleteFrames(s, untilPresentationTimeStamp: .invalid)
            VTCompressionSessionInvalidate(s)
            self?.lock.lock()
            self?.isTearingDown = false
            self?.lock.unlock()
            print("[ius] h264 encoder invalidated")
        }
    }

    public func endSession() {
        teardownSession()
    }

    // ---- packets -----------------------------------------------------------

    private func isSyncFrame(_ sb: CMSampleBuffer) -> Bool {
        if let arr = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false)
            as? [[String: Any]], let first = arr.first {
            let depends = first[kCMSampleAttachmentKey_DependsOnOthers as String] as? Bool ?? true
            return !depends
        }
        return false
    }

    private func handleEncoded(sampleBuffer: CMSampleBuffer) {
        guard let desc = CMSampleBufferGetFormatDescription(sampleBuffer) else { return }
        guard let ext = CMFormatDescriptionGetExtensions(desc) as? [String: Any],
              let atoms = ext["SampleDescriptionExtensionAtoms"] as? [String: Any],
              let rawAtom = atoms["avcC"] as? Data, rawAtom.count > 8 else { return }

        // Some SDKs wrap avcC with a size+fourcc prefix.
        var newAvcC = rawAtom
        if newAvcC.count >= 8,
           Int(newAvcC[newAvcC.startIndex]) << 24 |
           Int(newAvcC[newAvcC.startIndex+1]) << 16 |
           Int(newAvcC[newAvcC.startIndex+2]) << 8 |
           Int(newAvcC[newAvcC.startIndex+3]) == newAvcC.count,
           newAvcC[newAvcC.startIndex+4...newAvcC.startIndex+7].elementsEqual("avcC".utf8) {
            newAvcC = newAvcC.dropFirst(8)
        }

        var payload = Data()
        if let cb = CMSampleBufferGetDataBuffer(sampleBuffer) {
            var len = 0
            var ptr: UnsafeMutablePointer<Int8>?
            CMBlockBufferGetDataPointer(cb, atOffset: 0, lengthAtOffsetOut: nil,
                                        totalLengthOut: &len, dataPointerOut: &ptr)
            if len > 0 {
                payload = Data(count: len)
                payload.withUnsafeMutableBytes { raw in
                    _ = CMBlockBufferCopyDataBytes(cb, atOffset: 0, dataLength: len,
                                                   destination: raw.baseAddress!)
                }
            }
        }
        guard !payload.isEmpty else { return }

        let pts = CMSampleBufferGetOutputPresentationTimeStamp(sampleBuffer)
        let absTicks = Int64((CMTimeGetSeconds(pts) * 60000.0).rounded())

        lock.lock()
        if baseTicks == nil { baseTicks = absTicks }
        let ticks = absTicks - (baseTicks ?? absTicks)
        let isSync = isSyncFrame(sampleBuffer)

        let avcChanged = (avcC != newAvcC)
        if avcChanged {
            avcC = newAvcC
            initSegment = MP4.buildInit(avcC: newAvcC, width: encWidth, height: encHeight)
        }
        var duration: UInt32 = 1000
        if let prev = lastTicks {
            let delta = ticks - prev
            if delta > 0 { duration = UInt32(min(delta, Int64(Int32.max))) }
        }
        lastTicks = ticks
        bytesWindow += payload.count
        seq &+= 1
        let curSeq = seq
        let initSegSnapshot = initSegment
        let baseTicksNow = UInt64(max(0, ticks))
        lock.unlock()

        let codecStr = MP4.codecString(avcC: newAvcC) ?? "avc1.640028"
        let frag = MP4.buildFragment(seq: curSeq,
                                     baseDecodeTicks: baseTicksNow,
                                     duration: duration,
                                     sampleSize: payload.count,
                                     isSync: isSync,
                                     payload: payload)

        lock.lock()
        var targets: [(WebSocketConn, Bool)] = []
        for i in clients.indices {
            let needsInit = !clients[i].joined
            if needsInit {
                guard isSync else { continue }
                clients[i].joined = true
            }
            targets.append((clients[i].ws, needsInit))
        }
        lock.unlock()

        for (ws, newlyJoined) in targets {
            if newlyJoined {
                let avcCB64 = newAvcC.base64EncodedString()
                ws.enqueue(opcode: 0x1, payload: Data("{\"codec\":\"\(codecStr)\",\"avcC\":\"\(avcCB64)\",\"width\":\(encWidth),\"height\":\(encHeight)}".utf8))
                if let seg = initSegSnapshot { ws.enqueue(opcode: 0x2, payload: seg) }
            }
            ws.enqueue(opcode: 0x2, payload: frag)
        }
    }
}

// ---------------------------------------------------------------------------
// Client transport + viewer fanout
// ---------------------------------------------------------------------------
extension H264Stream {
    func addWebSocket(_ conn: NWConnection, initialBuffer: Data = Data()) {
        let ws = WebSocketConn(conn: conn, initialBuffer: initialBuffer) { [weak self] in
            self?.remove(conn)
        }
        ws.onMessage = { [weak self] isText, data in
            guard let self, isText else { return }
            // Best-effort JSON parse — do not crash on malformed data.
            guard let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
            else { return }
            let op = obj["op"] as? String ?? ""
            if op == "tune" || op == "query" {
                // Defer off the WebSocket receive-loop so VT callbacks and
                // enqueue/pump on the encoder pipeline cannot block each other.
                DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                    guard let self else { return }
                    // Bail if the connection was closed between receiving the
                    // message and reaching this point.
                    guard !ws.isClosed else { return }
                    self.handleControl(obj, reply: ws)
                }
            } else if obj["kind"] != nil {
                let reply = WdaRelay.shared.handle(message: obj)
                if let d = try? JSONSerialization.data(withJSONObject: reply) {
                    ws.enqueue(opcode: 0x1, payload: d)
                }
            }
        }
        lock.lock()
        clients.append(Client(ws: ws, joined: false))
        let n = clients.count
        let needForce = n == 1 || session != nil
        if needForce { pendingForceKeyFrame = true }
        lock.unlock()
        print("[ius] h264 viewer connected (\(n) total)")
    }

    private func remove(_ conn: NWConnection) {
        DispatchQueue.global().async { [weak self] in
            guard let self else { return }
            self.lock.lock()
            if let i = self.clients.firstIndex(where: { $0.ws.conn === conn }) {
                self.clients.remove(at: i)
            }
            let n = self.clients.count
            self.lock.unlock()
            print("[ius] h264 viewer disconnected (\(n) total)")
            if n == 0 { self.endSession() }
        }
    }

    /// Control messages from viewers ({"op":"tune", ...} / {"op":"query"}).
    private func handleControl(_ obj: [String: Any], reply: WebSocketConn?) {
        guard let op = obj["op"] as? String else { return }
        switch op {
        case "query":
            let t = currentTuning()
            let body: [String: Any] = [
                "op": "tuning",
                "bitrateMbps": t.bitrateMbps, "maxFps": t.maxFps,
                "scale": t.scale, "keyframeSeconds": t.keyframeSeconds,
            ]
            if let data = try? JSONSerialization.data(withJSONObject: body) {
                reply?.enqueue(opcode: 0x1, payload: data)
            }
        case "tune":
            var t = currentTuning()
            if let b = obj["bitrateMbps"] as? NSNumber { t.bitrateMbps = b.doubleValue }
            if let f = obj["maxFps"] as? NSNumber { t.maxFps = f.doubleValue }
            if let s = obj["scale"] as? NSNumber { t.scale = s.doubleValue }
            if let k = obj["keyframeSeconds"] as? NSNumber { t.keyframeSeconds = k.doubleValue }
            let applied = applyTuning(t)
            let body: [String: Any] = [
                "op": "tuning", "applied": true,
                "bitrateMbps": applied.bitrateMbps, "maxFps": applied.maxFps,
                "scale": applied.scale, "keyframeSeconds": applied.keyframeSeconds,
            ]
            if let data = try? JSONSerialization.data(withJSONObject: body) {
                broadcastText(data)
            }
        default:
            break
        }
    }

    private func broadcastText(_ data: Data) {
        lock.lock()
        let targets = clients.map { $0.ws }
        lock.unlock()
        for ws in targets { ws.enqueue(opcode: 0x1, payload: data) }
    }
}

// ---------------------------------------------------------------------------
// GPU scaler — shared Core Image context, reused scratch buffers.
// ---------------------------------------------------------------------------
final class GPUScaler {
    static let shared = GPUScaler()

    private let ciContext: CIContext
    private var pool: CVPixelBufferPool?
    private var poolW = 0, poolH = 0

    private init() {
        if let dev = MTLCreateSystemDefaultDevice() {
            ciContext = CIContext(mtlDevice: dev)
        } else {
            ciContext = CIContext()
        }
    }

    func scale(_ src: CVPixelBuffer, to w: Int, _ h: Int) -> CVPixelBuffer? {
        let ci = CIImage(cvPixelBuffer: src)
        if poolW != w || poolH != h {
            let attrs: [CFString: Any] = [
                kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
                kCVPixelBufferWidthKey: w,
                kCVPixelBufferHeightKey: h,
                kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
            ]
            var p: CVPixelBufferPool?
            CVPixelBufferPoolCreate(nil, nil, attrs as CFDictionary, &p)
            pool = p
            poolW = w; poolH = h
        }
        guard let pool else { return nil }
        var out: CVPixelBuffer?
        CVPixelBufferPoolCreatePixelBuffer(nil, pool, &out)
        guard let ob = out else { return nil }
        let rect = CGRect(x: 0, y: 0, width: w, height: h)
        let scaled = ci
            .applyingFilter("CILanczosScaleTransform", parameters: [
                kCIInputScaleKey: min(Double(w) / Double(CVPixelBufferGetWidth(src)),
                                      Double(h) / Double(CVPixelBufferGetHeight(src))),
            ])
        ciContext.render(scaled, to: ob, bounds: rect, colorSpace: CGColorSpaceCreateDeviceRGB())
        return ob
    }
}

extension H264Stream {
    static let playerHTML = """
<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Meridian Relay — Live</title>
<style>
  :root { --bg:#0e0e13; --panel:#16161d; --line:#262633; --fg:#e6e6ef; --dim:#9d9db0; --acc:#4f8eff; --warn:#ffb340; }
  * { box-sizing:border-box; margin:0; padding:0 }
  body { background:var(--bg); color:var(--fg); font-family:ui-sans-serif,system-ui,-apple-system,sans-serif; height:100vh; display:flex; flex-direction:column; overflow:hidden; }
  header { display:flex; align-items:center; gap:14px; padding:10px 16px; background:var(--panel); border-bottom:1px solid var(--line); flex-shrink:0; }
  header h1 { font-size:15px; font-weight:600; letter-spacing:.2px }
  .chip { display:inline-flex; align-items:center; gap:6px; font-size:12px; color:var(--dim); background:#0f0f15; border:1px solid var(--line); padding:3px 9px; border-radius:999px; }
  .chip b { color:var(--fg); font-weight:600; }
  .chip .dot { width:7px; height:7px; border-radius:50%; background:var(--warn); }
  .chip.live .dot { background:#4ade80; box-shadow:0 0 6px #4ade8088; }
  .spacer { flex:1 }
  button, select { background:#1e1e28; color:var(--fg); border:1px solid var(--line); border-radius:8px; font:inherit; font-size:13px; padding:6px 10px; cursor:pointer; }
  button:hover, select:hover { border-color:var(--acc) }
  main { position:relative; flex:1; display:flex; align-items:center; justify-content:center; background:#000; min-height:0; }
  video, canvas { max-width:100%; max-height:100%; outline:none; object-fit:contain; }
  #toast { position:absolute; top:12px; left:50%; transform:translateX(-50%); background:rgba(22,22,29,.92); border:1px solid var(--line); color:var(--fg); border-radius:10px; padding:7px 14px; font-size:13px; opacity:0; transition:opacity .18s; pointer-events:none; }
  #toast.show { opacity:1 }
  #panel { position:absolute; right:12px; top:12px; width:230px; background:rgba(20,20,28,.96); border:1px solid var(--line); border-radius:12px; padding:12px; transform:translateX(0); transition:transform .18s, opacity .18s; backdrop-filter: blur(8px); }
  #panel.closed { transform:translateX(calc(100% + 14px)); opacity:0; pointer-events:none; }
  #panel h2 { font-size:12px; text-transform:uppercase; letter-spacing:.08em; color:var(--dim); margin-bottom:10px; }
  .row { display:flex; align-items:center; justify-content:space-between; gap:8px; margin:8px 0; }
  .row label { font-size:12px; color:var(--dim) }
  .row input[type=range] { flex:1; accent-color:var(--acc) }
  .row .val { font-variant-numeric:tabular-nums; font-size:12px; color:var(--fg); width:52px; text-align:right }
  .seg { display:flex; gap:4px }
  .seg button { padding:4px 8px; font-size:12px }
  .seg button.on { background:var(--acc); border-color:var(--acc); color:#fff }
</style></head>
<body>
<header>
  <h1>Meridian Relay</h1>
  <span id="statLive" class="chip"><span class="dot"></span><b id="statCodec">h264</b></span>
  <span class="chip">bitrate <b id="statMbps">–</b></span>
  <span class="chip">fps <b id="statFps">–</b></span>
  <span class="chip">latency <b id="statLat">–</b></span>
  <div class="spacer"></div>
  <button id="btnTune" title="Stream settings (S)">⚙ Tune</button>
  <button id="btnFs" title="Fullscreen (F)">⤢ Fullscreen</button>
</header>
<main id="stage">
  <canvas id="c"></canvas>
  <video id="v" autoplay muted playsinline webkit-playsinline disablepictureinpicture style="display:none"></video>
  <div id="toast"></div>
  <aside id="panel" class="closed">
    <h2>Stream settings</h2>
    <div class="row"><label>Bitrate</label><input id="rBitrate" type="range" min="0.5" max="12" step="0.5"><span class="val" id="vBitrate"></span></div>
    <div class="row"><label>Frame rate</label><div class="seg" id="segFps"></div></div>
    <div class="row"><label>Scale</label><div class="seg" id="segScale"></div></div>
    <div class="row"><label>Keyframe</label><div class="seg" id="segKey"></div></div>
    <div class="row" style="margin-top:12px"><button id="btnSave" style="flex:1">Apply</button></div>
  </aside>
</main>
<script>
(() => {
  const v = document.getElementById('v'), c = document.getElementById('c'), toast = document.getElementById('toast');
  const statLive = document.getElementById('statLive'), statMbps = document.getElementById('statMbps');
  const statFps = document.getElementById('statFps'), statLat = document.getElementById('statLat');
  const panel = document.getElementById('panel');
  let ws, ms, sb, retryT, codec = null;
  let pending = [], appending = false;
  let rxBytes = 0, rxStamp = performance.now(), fpsCount = 0, fpsStamp = performance.now();
  let webCodecsDecoder = null;
  let ctx2d = null;
  const hasWebCodecs = typeof window.VideoDecoder === 'function';

  // ---- stats ---------------------------------------------------------------
  const fmtMbps = b => (b * 8 / 1e6).toFixed(2);
  setInterval(() => {
    const now = performance.now();
    const dt = (now - rxStamp) / 1000; rxStamp = now;
    statMbps.textContent = fmtMbps(rxBytes / dt);
    const fdt = (now - fpsStamp) / 1000; fpsStamp = now;
    statFps.textContent = Math.round(fpsCount / fdt);
    rxBytes = 0; fpsCount = 0;
  }, 800);

  // Ping device periodically to monitor physical transport RTT
  setInterval(() => {
    if (ws && ws.readyState === WebSocket.OPEN) {
      try { ws.send(JSON.stringify({ kind: 'ping', t: performance.now() })); } catch(e){}
    }
  }, 800);

  function toastMsg(t) {
    toast.textContent = t; toast.classList.add('show');
    clearTimeout(toast._t); toast._t = setTimeout(() => toast.classList.remove('show'), 1600);
  }

  // ---- WebCodecs Pipeline (Zero Buffer Delay, 60fps) -----------------------
  function initWebCodecs(m) {
    if (!hasWebCodecs) {
      fallbackToMSE(m);
      return;
    }
    if (webCodecsDecoder) {
      try { webCodecsDecoder.close(); } catch(e){}
      webCodecsDecoder = null;
    }
    ctx2d = c.getContext('2d', { alpha: false, desynchronized: true });
    webCodecsDecoder = new VideoDecoder({
      output: frame => {
        if (c.width !== frame.displayWidth || c.height !== frame.displayHeight) {
          c.width = frame.displayWidth;
          c.height = frame.displayHeight;
        }
        ctx2d.drawImage(frame, 0, 0);
        frame.close();
        fpsCount++;
      },
      error: err => {
        console.warn('WebCodecs error, switching to MSE fallback:', err);
        fallbackToMSE(m);
      }
    });

    try {
      const desc = Uint8Array.from(atob(m.avcC), ch => ch.charCodeAt(0));
      webCodecsDecoder.configure({
        codec: m.codec,
        description: desc,
        optimizeForLatency: true
      });
      c.style.display = 'block';
      v.style.display = 'none';
      statLive.classList.add('live');
      document.getElementById('statCodec').textContent = m.codec + ' (gpu)';
    } catch(e) {
      console.warn('WebCodecs config failed:', e);
      fallbackToMSE(m);
    }
  }

  function fallbackToMSE(m) {
    if (webCodecsDecoder) {
      try { webCodecsDecoder.close(); } catch(e){}
      webCodecsDecoder = null;
    }
    c.style.display = 'none';
    v.style.display = 'block';
    openMSE('video/mp4; codecs="' + m.codec + '"');
    document.getElementById('statCodec').textContent = m.codec + ' (mse)';
  }

  // ---- MSE Pipeline (Smooth Continuous Fallback) ---------------------------
  let initialSeekDone = false;
  function openMSE(mime) {
    if (ms) return;
    initialSeekDone = false;
    ms = new MediaSource();
    ms.addEventListener('sourceopen', () => {
      try {
        sb = ms.addSourceBuffer(mime);
        sb.mode = 'segments';
        sb.addEventListener('updateend', () => {
          drain();
          if (!initialSeekDone && sb.buffered.length > 0) {
            const end = sb.buffered.end(sb.buffered.length - 1);
            if (end > 0.05) {
              v.currentTime = end;
              initialSeekDone = true;
            }
          }
        });
        drain();
        statLive.classList.add('live');
      } catch (e) { toastMsg('MSE unsupported: ' + e.message); }
    });
    v.src = URL.createObjectURL(ms);
  }

  function drain() {
    if (!sb || sb.updating || appending) return;
    const item = pending.shift();
    if (!item) return;
    appending = true;
    try { sb.appendBuffer(item); } catch (e) { toastMsg('append error: ' + e.message); }
    appending = false;
  }

  // MSE smooth drift control - NO violent seeking, NO playbackRate oscillation
  function smoothLiveEdge() {
    if (!webCodecsDecoder && sb && sb.buffered.length > 0) {
      const end = sb.buffered.end(sb.buffered.length - 1);
      const behind = end - v.currentTime;
      statLat.textContent = Math.max(0, Math.round(behind * 1000)) + ' ms';

      if (behind > 1.0) {
        v.currentTime = end - 0.05;
        v.playbackRate = 1.0;
      } else if (behind > 0.12) {
        v.playbackRate = 1.08;
      } else {
        v.playbackRate = 1.0;
      }
      if (v.paused || v.ended) v.play().catch(() => {});
      if (behind > 6 && !sb.updating && !appending) {
        try { sb.remove(sb.buffered.start(0), end - 2.0); } catch (e) {}
      }
    }
  }
  setInterval(smoothLiveEdge, 200);

  // ---- websocket -----------------------------------------------------------
  function connect() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    ws = new WebSocket(proto + '://' + location.host + '/stream.ws');
    ws.binaryType = 'arraybuffer';

    ws.onopen = () => {
      toastMsg('connected');
      queryTuning();
    };
    ws.onclose = () => {
      statLive.classList.remove('live');
      toastMsg('disconnected — retrying');
      retryT = setTimeout(connect, 800);
    };
    ws.onerror = () => {};
    ws.onmessage = ev => {
      if (typeof ev.data === 'string') {
        let m; try { m = JSON.parse(ev.data); } catch { return; }
        if (m.codec) {
          codec = m.codec;
          if (hasWebCodecs && m.avcC) {
            initWebCodecs(m);
          } else {
            fallbackToMSE(m);
          }
        }
        if (m.op === 'tuning') fillTuning(m);
        if (m.t) {
          const rtt = Math.round(performance.now() - m.t);
          statLat.textContent = rtt + ' ms';
        }
      } else {
        const data = new Uint8Array(ev.data);
        rxBytes += data.byteLength;

        if (webCodecsDecoder && webCodecsDecoder.state === 'configured') {
          if (data.length < 8) return;
          const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
          const tag = String.fromCharCode(data[4], data[5], data[6], data[7]);
          if (tag === 'ftyp') return;
          if (tag === 'moof') {
            const moofLen = view.getUint32(0);
            if (moofLen + 8 > data.length) return;
            const naluData = new Uint8Array(data.buffer, data.byteOffset + moofLen + 8, data.byteLength - moofLen - 8);
            if (naluData.length < 5) return;

            let isKey = false;
            let pos = 0;
            while (pos + 4 < naluData.length) {
              const nalLen = (naluData[pos] << 24) | (naluData[pos+1] << 16) | (naluData[pos+2] << 8) | naluData[pos+3];
              const nalType = naluData[pos + 4] & 0x1F;
              if (nalType === 5 || nalType === 7) {
                isKey = true;
                break;
              }
              pos += 4 + nalLen;
            }

            try {
              webCodecsDecoder.decode(new EncodedVideoChunk({
                type: isKey ? 'key' : 'delta',
                timestamp: performance.now() * 1000,
                data: naluData
              }));
            } catch(e) {
              console.warn('decode error:', e);
            }
            return;
          }
        }

        // MSE path
        fpsCount++;
        pending.push(data);
        drain();
      }
    };
  }
  connect();

  // ---- tuning panel --------------------------------------------------------
  const state = { bitrateMbps: 6, maxFps: 0, scale: 1, keyframeSeconds: 1 };
  document.getElementById('btnTune').onclick = () => panel.classList.toggle('closed');
  document.getElementById('btnFs').onclick = () => {
    const el = webCodecsDecoder ? c : v;
    if (document.fullscreenElement) document.exitFullscreen(); else el.requestFullscreen();
  };

  const rBitrate = document.getElementById('rBitrate');
  const vBitrate = document.getElementById('vBitrate');
  const segFps = document.getElementById('segFps');
  const segScale = document.getElementById('segScale');
  const segKey = document.getElementById('segKey');

  function segInit(el, opts, cur, fmt, cb) {
    el.innerHTML = '';
    for (const o of opts) {
      const b = document.createElement('button');
      b.textContent = fmt(o);
      b.classList.toggle('on', o === cur);
      b.onclick = () => { cb(o); segInit(el, opts, o, fmt, cb); };
      el.appendChild(b);
    }
  }
  rBitrate.oninput = () => { state.bitrateMbps = parseFloat(rBitrate.value); vBitrate.textContent = rBitrate.value + ' M'; };
  segInit(segFps, [0, 30, 60], 0, v => v === 0 ? 'max' : v, v => state.maxFps = v);
  segInit(segScale, [0.5, 0.75, 1.0], 1, v => (v * 100) + '%', v => state.scale = v);
  segInit(segKey, [0.5, 1, 2], 1, v => v + 's', v => state.keyframeSeconds = v);

  function fillTuning(m) {
    Object.assign(state, {
      bitrateMbps: m.bitrateMbps ?? state.bitrateMbps,
      maxFps: m.maxFps ?? state.maxFps,
      scale: m.scale ?? state.scale,
      keyframeSeconds: m.keyframeSeconds ?? state.keyframeSeconds,
    });
    rBitrate.value = state.bitrateMbps; vBitrate.textContent = state.bitrateMbps + ' M';
    segInit(segFps, [0, 30, 60], state.maxFps, v => v === 0 ? 'max' : v, v => state.maxFps = v);
    segInit(segScale, [0.5, 0.75, 1.0], state.scale, v => (v * 100) + '%', v => state.scale = v);
    segInit(segKey, [1, 2, 5], state.keyframeSeconds, v => v + 's', v => state.keyframeSeconds = v);
  }

  document.getElementById('btnSave').onclick = () => {
    ws.send(JSON.stringify({
      op: 'tune', bitrateMbps: state.bitrateMbps, maxFps: state.maxFps,
      scale: state.scale, keyframeSeconds: state.keyframeSeconds,
    }));
    toastMsg('applied: ' + state.bitrateMbps + ' Mbps · ' + (state.maxFps || 'max') + ' fps · ' + Math.round(state.scale * 100) + '%');
  };

  function queryTuning() { try { ws.send(JSON.stringify({ op: 'query' })); } catch {} }

  document.addEventListener('keydown', e => {
    if (e.key === 'f' || e.key === 'F') { if (document.fullscreenElement) document.exitFullscreen(); else v.requestFullscreen(); }
    if (e.key === 's' || e.key === 'S') panel.classList.toggle('closed');
  });
})();
</script>
</body></html>
"""
}
