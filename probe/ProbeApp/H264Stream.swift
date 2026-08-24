import AVFoundation
import CoreMedia
import CryptoKit
import Network
import VideoToolbox

// ---------------------------------------------------------------------------
// Minimal fMP4 box builders (H.264/AVCC, single video track)
// ---------------------------------------------------------------------------
private enum MP4 {
    static func u16(_ v: UInt16) -> Data { withUnsafeBytes(of: v.bigEndian) { Data($0) } }
    static func u32(_ v: UInt32) -> Data { withUnsafeBytes(of: v.bigEndian) { Data($0) } }

    static func box(_ type: String, _ payload: Data) -> Data {
        var out = Data()
        out.append(u32(UInt32(payload.count + 8)))
        out.append(Data(type.utf8))
        out.append(payload)
        return out
    }

    static func fullbox(_ type: String, _ version: UInt8, _ flags: UInt32,
                        _ payload: Data) -> Data {
        var head = Data([version])
        head.append(u32(flags))
        return box(type, head + payload)
    }

    static let unityMatrix: Data = {
        var d = Data()
        for v: UInt32 in [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
            d.append(u32(v))
        }
        return d
    }()

    /// Init segment: ftyp + moov + mvex (zero durations; fragmented stream).
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

    /// One-sample media fragment: moof (patched data_offset) + mdat.
    static func buildFragment(seq: UInt32, duration: UInt32,
                              sampleSize: Int, isSync: Bool,
                              payload: Data) -> Data {
        let flags: UInt32 = isSync ? 0x02000000 : 0x01010000

        var trunPayload = Data()
        trunPayload.append(u32(1))
        trunPayload.append(u32(0xDEADBEEF))          // data_offset placeholder
        trunPayload.append(u32(duration))
        trunPayload.append(u32(UInt32(sampleSize)))
        trunPayload.append(u32(flags))
        let trun = fullbox("trun", 0, 0x000701, trunPayload)

        let tfhd = fullbox("tfhd", 0, 0x020000, u32(1))   // default-base-is-moof
        let mfhd = fullbox("mfhd", 0, 0, u32(seq))
        let traf = box("traf", tfhd + trun)
        var moof = box("moof", mfhd + traf)

        let mdat = box("mdat", payload)

        if let idx = moof.range(of: Data([0xDE, 0xAD, 0xBE, 0xEF])) {
            let off = UInt32(moof.count + 8)
            moof.replaceSubrange(idx, with: u32(off))
        }
        return moof + mdat
    }

    /// "avc1.XXYYZZ" derived from SPS profile/compat/level bytes inside avcC.
    static func codecString(avcC: Data) -> String? {
        guard avcC.count >= 6 else { return nil }
        let i = avcC.startIndex
        return String(format: "avc1.%02X%02X%02X",
                      avcC[i + 1], avcC[i + 2], avcC[i + 3])
    }
}

// ---------------------------------------------------------------------------
// Hardware H.264 encoder fed by SCK samples -> fMP4 -> WebSocket viewers
// ---------------------------------------------------------------------------
final class H264Stream {
    static let shared = H264Stream()

    struct Client {
        let ws: WebSocketConn
        var joined: Bool      // has received init segment + first keyframe
    }

    private let lock = NSLock()
    private var clients: [Client] = []

    private var session: VTCompressionSession?
    private var encWidth = 0
    private var encHeight = 0
    private var avcC: Data?
    private var initSegment: Data?
    private var seq: UInt32 = 0
    private var lastTicks: Int64?
    private var baseTicks: Int64?
    private var pendingForceKeyFrame = false
    private var endingSession = false

    var hasViewers: Bool {
        lock.lock(); defer { lock.unlock() }
        return !clients.isEmpty
    }

    // ---- client management -------------------------------------------------

    func addWebSocket(_ conn: NWConnection) {
        let ws = WebSocketConn(conn: conn) { [weak self] in
            self?.remove(conn)
        }
        lock.lock()
        clients.append(Client(ws: ws, joined: false))
        let n = clients.count
        let needForce = n == 1 || session != nil   // mid-stream joiner wants IDR now
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

    // ---- encoding ----------------------------------------------------------

    func push(sampleBuffer: CMSampleBuffer) {
        guard hasViewers else { return }
        guard let pb = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let w = CVPixelBufferGetWidth(pb)
        let h = CVPixelBufferGetHeight(pb)
        ensureSession(width: w, height: h)

        var props: CFDictionary?
        lock.lock()
        if pendingForceKeyFrame {
            props = [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary
            pendingForceKeyFrame = false
        }
        lock.unlock()

        guard let session = session else { return }
        let pts = CMSampleBufferGetOutputPresentationTimeStamp(sampleBuffer)
        VTCompressionSessionEncodeFrame(session,
                                        imageBuffer: pb,
                                        presentationTimeStamp: pts,
                                        duration: .invalid,
                                        frameProperties: props,
                                        sourceFrameRefcon: nil,
                                        infoFlagsOut: nil)
    }

    private func ensureSession(width: Int, height: Int) {
        lock.lock()
        if session != nil || endingSession {
            lock.unlock()
            return
        }
        endingSession = true
        lock.unlock()

        var session: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            width: Int32(width), height: Int32(height),
            codecType: kCMVideoCodecType_H264,
            encoderSpecification: [
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: true,
            ] as CFDictionary,
            imageBufferAttributes: nil,
            compressedDataAllocator: nil,
            outputCallback: { refCon, _, status, _, sampleBuffer in
                guard status == noErr, let sb = sampleBuffer, let refCon else { return }
                let me = Unmanaged<H264Stream>.fromOpaque(refCon).takeUnretainedValue()
                me.handleEncoded(sampleBuffer: sb)
            },
            refcon: Unmanaged.passUnretained(self).toOpaque(),
            compressionSessionOut: &session)

        guard status == noErr, let s = session else {
            print("[ius] h264 encoder create failed: \(status)")
            lock.lock(); endingSession = false; lock.unlock()
            return
        }
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_RealTime,
                             value: kCFBooleanTrue)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_AverageBitRate,
                             value: 10_000_000 as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_ExpectedFrameRate,
                             value: 60 as CFNumber)
        VTSessionSetProperty(s, key: kVTCompressionPropertyKey_MaxKeyFrameInterval,
                             value: 120 as CFNumber)
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
        endingSession = false
        lock.unlock()
        print("[ius] hw h264 encoder running \(width)x\(height)")
    }

    func endSession() {
        lock.lock()
        guard !endingSession else { lock.unlock(); return }
        let s = session
        session = nil
        endingSession = true
        lock.unlock()
        _ = s
        lock.lock(); session = nil; endingSession = false; lock.unlock()
        print("[ius] hw h264 encoder stopped")
    }

    private func dependsOnOthersFalse(_ sb: CMSampleBuffer) -> Bool {
        if let arr = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false)
            as? [[String: Any]],
           let first = arr.first {
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
        // SDK may hand us the complete atom (size+fourcc+payload); normalize
        var newAvcC = rawAtom
        if newAvcC.count >= 8,
           Int(newAvcC[newAvcC.startIndex]) << 24 |
             Int(newAvcC[newAvcC.startIndex+1]) << 16 |
             Int(newAvcC[newAvcC.startIndex+2]) << 8 |
             Int(newAvcC[newAvcC.startIndex+3]) == newAvcC.count,
           newAvcC[newAvcC.startIndex+4...newAvcC.startIndex+7].elementsEqual("avcC".utf8) {
            newAvcC = newAvcC.dropFirst(8)
        }

        var size = 0
        if let cb = CMSampleBufferGetDataBuffer(sampleBuffer) {
            var len = 0
            var ptr: UnsafeMutablePointer<Int8>?
            CMBlockBufferGetDataPointer(cb, atOffset: 0, lengthAtOffsetOut: nil,
                                        totalLengthOut: &len, dataPointerOut: &ptr)
            size = len
        }
        guard size > 0 else { return }
        var payload = Data(count: size)
        payload.withUnsafeMutableBytes { raw in
            _ = CMBlockBufferCopyDataBytes(
                CMSampleBufferGetDataBuffer(sampleBuffer)!,
                atOffset: 0, dataLength: size, destination: raw.baseAddress!)
        }

        let pts = CMSampleBufferGetOutputPresentationTimeStamp(sampleBuffer)
        let absTicks = Int64((CMTimeGetSeconds(pts) * 60000.0).rounded())
        lock.lock()
        if baseTicks == nil { baseTicks = absTicks }
        let ticks = absTicks - (baseTicks ?? absTicks)
        lock.unlock()
        let isSync = dependsOnOthersFalse(sampleBuffer)

        lock.lock()
        let changed = (avcC != newAvcC)
        if changed {
            avcC = newAvcC
            initSegment = MP4.buildInit(avcC: newAvcC,
                                        width: encWidth, height: encHeight)
        }
        var duration: UInt32 = 1000
        if let prev = lastTicks {
            let delta = ticks - prev
            if delta > 0 { duration = UInt32(min(delta, Int64(Int32.max))) }
        }
        lastTicks = ticks
        seq &+= 1
        let curSeq = seq
        let initSeg = initSegment

        // Snapshot targets under lock; enqueue OUTSIDE the lock.
        // (An overflowing viewer triggers close()->remove() which re-enters
        // this lock - holding it during enqueues would deadlock.)
        let codecStr = MP4.codecString(avcC: newAvcC) ?? "avc1.640028"
        let frag = MP4.buildFragment(seq: curSeq, duration: duration,
                                     sampleSize: size, isSync: isSync,
                                     payload: payload)
        let initSegSnapshot = initSegment

        var targets: [(WebSocketConn, Bool)] = []
        for i in clients.indices {
            targets.append((clients[i].ws, !clients[i].joined))
            if !clients[i].joined { clients[i].joined = true }
        }
        lock.unlock()

        var joinedCount = 0
        for (ws, newlyJoined) in targets {
            if newlyJoined {
                guard isSync else { continue }          // late joiner waits for IDR
                ws.enqueue(opcode: 0x1,
                    payload: Data("{\"codec\":\"\(codecStr)\"}".utf8))
                if let seg = initSegSnapshot { ws.enqueue(payload: seg) }
                joinedCount += 1
                print("[ius] h264 viewer joined stream")
            }
            ws.enqueue(payload: frag)
        }
        if joinedCount > 0 { print("[ius] joined count: \(joinedCount)") }
    }
}

extension H264Stream {
    static let playerHTML = """
<!doctype html>
<html><head><meta charset="utf-8"><title>IUS live - H.264</title>
<style>body{background:#0b0b0f;color:#ddd;font-family:ui-monospace,monospace;text-align:center;margin:0;padding:12px}
video{max-width:100%;max-height:86vh;background:#000;border-radius:6px}
#s{opacity:.7;font-size:13px;margin-top:8px}</style></head>
<body>
<h3>IUS SCK - hardware H.264 / fMP4</h3>
<video id="v" autoplay muted playsinline></video>
<div id="s">connecting...</div>
<script>
const v = document.getElementById('v'), st = document.getElementById('s');
let ws = null, msb = null, sb = null, queue = [], codec = '';

function setStatus(t){ st.textContent = t; }

function openMSE(){
  if (msb) return;
  setStatus('opening MSE (' + codec + ')');
  msb = new MediaSource();
  msb.addEventListener('sourceopen', onSourceOpen);
  v.src = URL.createObjectURL(msb);
}

function onSourceOpen(){
  setStatus('MSE open');
  sb = msb.addSourceBuffer('video/mp4; codecs="' + codec + '"');
  sb.mode = 'segments';
  sb.addEventListener('updateend', () => { pump(); trim(); live(); });
  sb.addEventListener('error', () => {
    setStatus('decoder error - reloading');
    setTimeout(() => location.reload(), 900);
  });
  pump();
}

function pump(){
  if (!sb || sb.updating) return;
  const c = queue.shift();
  if (c === undefined) return;
  try { sb.appendBuffer(c); }
  catch(e) { }
}

function trim(){
  try {
    if (sb && sb.buffered.length && sb.buffered.start(0) < sb.buffered.end(sb.buffered.length-1) - 30)
      sb.remove(0, sb.buffered.end(sb.buffered.length-1) - 15);
  } catch(e){}
}

function live(){
  try {
    if (sb && sb.buffered.length) {
      const end = sb.buffered.end(sb.buffered.length-1);
      if (v.currentTime < end - 2.5) v.currentTime = Math.max(0, end - 0.35);
      setStatus('live (buffer ' + Math.max(0, end - v.currentTime).toFixed(2) + 's)');
    }
  } catch(e){}
}

ws = new WebSocket((location.protocol === 'https:' ? 'wss' : 'ws') + '://' + location.host + '/stream.ws');
ws.binaryType = 'arraybuffer';

ws.onopen = () => setStatus('ws open - waiting for keyframe');
ws.onerror = () => setStatus('ws error');
ws.onclose = () => {
  setStatus('disconnected - retrying');
  setTimeout(() => location.reload(), 1200);
};

ws.onmessage = (e) => {
  if (typeof e.data === 'string') {
    const j = JSON.parse(e.data);
    if (!j.codec) return;
    codec = j.codec;
    openMSE();
  } else {
    if (queue.length > 240) queue.splice(0, 120);
    queue.push(new Uint8Array(e.data));
    if (!opened_flag()) setStatus('queued ' + queue.length);
    pump();
  }
};

function opened_flag(){ return msb !== null; }

setInterval(() => { pump(); trim(); live(); }, 500);
</script>
"""
}
