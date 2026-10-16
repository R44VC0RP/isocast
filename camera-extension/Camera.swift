// Isocast's CMIO camera extension. The client queue serializes all state;
// sink completions re-enter it. Samples already carry IOSurfaces: forwarding
// retains them through CMIO, never copies pixels or overwrites a held buffer.
import Foundation
import CoreMediaIO
import CoreVideo
import CoreGraphics
import CoreText
import IOKit.audio
import Security
import Darwin
import os

let log = Logger(subsystem: "dev.isocast.isocastd.camera", category: "producer")

// CMIO supplies signingID and pid (this SDK exposes no client audit token).
// On macOS 26.7 CMIO reports signingID "unknown" for the engine's producer,
// and the extension's sandbox denies code lookups of other processes by pid
// (SecCodeCopyGuestWithAttributes: EPERM). So: a reported id other than the
// engine's refuses; the live code's Apple anchor, identifier and team are
// checked whenever macOS allows the lookup, and a failed check refuses. The
// client id pins the reservation. Decisions are logged:
// `log stream --predicate 'subsystem == "dev.isocast.isocastd.camera"'`.
let producerRule = #"anchor apple generic and identifier "dev.isocast.isocastd" and certificate leaf[subject.OU] = "9G68SMNHEU""#

func producerCodeAllowed(signingID: String?, pid: pid_t) -> Bool {
    guard signingID == nil || signingID == "unknown" || signingID == "dev.isocast.isocastd" else {
        log.error("refused producer pid \(pid, privacy: .public): signing id \(signingID ?? "none", privacy: .public)")
        return false
    }
    var requirement: SecRequirement?
    guard SecRequirementCreateWithString(producerRule as CFString, SecCSFlags(rawValue: 0), &requirement) == errSecSuccess,
          let requirement else { return false }
    var code: SecCode?
    let attributes = [kSecGuestAttributePid: NSNumber(value: pid)] as CFDictionary
    let copied = SecCodeCopyGuestWithAttributes(nil, attributes, SecCSFlags(rawValue: 0), &code)
    if copied == errSecSuccess, let code {
        let valid = SecCodeCheckValidity(code, SecCSFlags(rawValue: 0), requirement)
        log.notice("producer pid \(pid, privacy: .public) live signature check \(valid, privacy: .public)")
        return valid == errSecSuccess
    }
    // Fallback (the sandbox allows proc_pidpath): an engine installed at
    // one of its exact paths, and its signature on disk when that can be read.
    var buffer = [CChar](repeating: 0, count: 4 * Int(MAXPATHLEN))
    guard proc_pidpath(pid, &buffer, UInt32(buffer.count)) > 0 else {
        log.error("refused producer pid \(pid, privacy: .public): code lookup denied (\(copied, privacy: .public)) and no path")
        return false
    }
    let path = String(cString: buffer)
    guard producerPathAllowed(path) else {
        log.error("refused producer pid \(pid, privacy: .public): \(path, privacy: .public) is not an installed Isocast engine")
        return false
    }
    var file: SecStaticCode?
    if SecStaticCodeCreateWithPath(URL(fileURLWithPath: path) as CFURL, SecCSFlags(rawValue: 0), &file) == errSecSuccess, let file {
        let valid = SecStaticCodeCheckValidity(file, SecCSFlags(rawValue: 0), requirement)
        log.notice("producer pid \(pid, privacy: .public) at \(path, privacy: .public): signature on disk \(valid, privacy: .public)")
        return valid == errSecSuccess
    }
    log.notice("producer pid \(pid, privacy: .public) at \(path, privacy: .public): accepted by path (code lookup denied: \(copied, privacy: .public))")
    return true
}

// Where an Isocast engine runs when installed (#69): the release app's
// helper, /Applications/<app>.app/Contents/Helpers/Isocast Engine.app/
// Contents/MacOS/Isocast Engine (#83; isocastd in older releases, still accepted
// so an older engine works until this build replaces its extension) (any
// name for the app, directly in /Applications), or the development host
// scripts/vcam-install.sh copies.
let releaseEngineSuffixes = [
    ".app/Contents/Helpers/Isocast Engine.app/Contents/MacOS/Isocast Engine",
    ".app/Contents/Helpers/Isocast Engine.app/Contents/MacOS/isocastd",
]
let devHostEngine = "/Applications/Isocast Camera Host.app/Contents/MacOS/isocastd"

func producerPathAllowed(_ path: String) -> Bool {
    if path == devHostEngine { return true }
    let prefix = "/Applications/"
    guard path.hasPrefix(prefix) else { return false }
    return releaseEngineSuffixes.contains { suffix in
        guard path.hasSuffix(suffix) else { return false }
        let name = path.dropFirst(prefix.count).dropLast(suffix.count)
        return !name.isEmpty && !name.contains("/")
    }
}

let cameraQueue = DispatchQueue(label: "dev.isocast.camera", qos: .userInteractive,
                                autoreleaseFrequency: .workItem)
let sizes: [(Int, Int)] = [(1920,1080), (1280,720), (2560,1440), (3840,2160), (1080,1920), (720,1280)]

func problem(_ message: String) -> NSError {
    NSError(domain: "dev.isocast.isocastd.camera", code: 1,
            userInfo: [NSLocalizedDescriptionKey: message])
}

// Negotiation is independent of CMIO notification delivery. Consumers keep
// preferences for standby; a connected producer supplies the effective format.
struct CameraFormatState {
    var inputIndex = 0
    var inputDuration = CMTime(value: 1, timescale: 60)
    private var requestedIndex = 0
    private var requestedDuration = CMTime(value: 1, timescale: 60)
    mutating func configure(index: Int?, duration: CMTime?, sink: Bool, producing: Bool) throws {
        let index = index ?? (sink ? inputIndex : requestedIndex)
        let duration = duration ?? (sink ? inputDuration : requestedDuration)
        guard sizes.indices.contains(index) else {
            throw problem("virtual_camera.width/height: choose an advertised format index 0–5")
        }
        guard duration.isValid, duration.seconds.isFinite, duration.seconds >= 1.0/240.0, duration.seconds <= 1 else {
            throw problem("virtual_camera.fps: choose a frame duration between 1/240 and 1 second")
        }
        if sink {
            guard !producing || (index == inputIndex && CMTimeCompare(duration, inputDuration) == 0) else {
                throw problem("virtual_camera: stop the current Isocast producer before changing its sink format")
            }
            inputIndex = index; inputDuration = duration
        } else {
            requestedIndex = index; requestedDuration = duration
        }
    }
    func effective(producing: Bool) -> (Int, CMTime) {
        producing ? (inputIndex, inputDuration) : (requestedIndex, requestedDuration)
    }
}

struct CameraPresentationClock {
    var last = CMTime.invalid
    private var cardWasSent = false
    private var liveOffset = CMTime.zero
    mutating func next(_ pts: CMTime, duration: CMTime, card: Bool) -> CMTime {
        if card {
            cardWasSent = true
            return last.isValid && CMTimeCompare(pts, last) <= 0 ? CMTimeAdd(last, duration) : pts
        }
        if cardWasSent || !last.isValid {
            liveOffset = .zero
            if last.isValid {
                let delta = CMTimeSubtract(CMTimeAdd(last, duration), pts)
                if delta.seconds > 0 { liveOffset = delta }
            }
            cardWasSent = false
        }
        return CMTimeAdd(pts, liveOffset)
    }
}

func tag(_ pixels: CVPixelBuffer) {
    CVBufferSetAttachment(pixels, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
    CVBufferSetAttachment(pixels, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, .shouldPropagate)
    CVBufferSetAttachment(pixels, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, .shouldPropagate)
}

func format(_ width: Int, _ height: Int) -> CMVideoFormatDescription {
    let tags: [CFString: Any] = [
        kCMFormatDescriptionExtension_ColorPrimaries: kCVImageBufferColorPrimaries_ITU_R_709_2,
        kCMFormatDescriptionExtension_TransferFunction: kCVImageBufferTransferFunction_ITU_R_709_2,
        kCMFormatDescriptionExtension_YCbCrMatrix: kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    ]
    var description: CMVideoFormatDescription?
    precondition(CMVideoFormatDescriptionCreate(allocator: nil, codecType: kCVPixelFormatType_32BGRA,
        width: Int32(width), height: Int32(height), extensions: tags as CFDictionary,
        formatDescriptionOut: &description) == noErr)
    return description!
}

final class CameraDevice: NSObject, CMIOExtensionDeviceSource {
    private(set) var device: CMIOExtensionDevice!
    private(set) var output: CameraStream!
    private(set) var input: CameraStream!
    var index = 0
    var duration = CMTime(value: 1, timescale: 60)
    private var negotiation = CameraFormatState()
    var inputIndex: Int { negotiation.inputIndex }
    var inputDuration: CMTime { negotiation.inputDuration }
    var viewers = 0
    var producer: CMIOExtensionClient?
    private var generation: UInt64 = 0
    private var lastArrival: UInt64 = 0
    private var clock = CameraPresentationClock()
    private var lastPTS: CMTime { get { clock.last } set { clock.last = newValue } }
    private var timer: DispatchSourceTimer?
    private var card: CVPixelBuffer?

    override init() {
        super.init()
        device = CMIOExtensionDevice(localizedName: "Isocast Virtual Camera",
            deviceID: UUID(uuidString: "D07A1962-6A48-4BD3-9074-DA9D25FC0141")!, legacyDeviceID: nil, source: self)
        output = CameraStream(self, .source, "E84BC672-697D-481D-812E-425D93011C56")
        input = CameraStream(self, .sink, "A7CC5314-A844-4BC0-8824-5EAC767E2B11")
        do { try device.addStream(output.stream); try device.addStream(input.stream) }
        catch { fatalError("Isocast streams: \(error)") }
    }

    var availableProperties: Set<CMIOExtensionProperty> { [.deviceTransportType, .deviceModel] }
    func deviceProperties(forProperties properties: Set<CMIOExtensionProperty>) throws -> CMIOExtensionDeviceProperties {
        let result = CMIOExtensionDeviceProperties(dictionary: [:])
        if properties.contains(.deviceTransportType) { result.transportType = kIOAudioDeviceTransportTypeVirtual }
        if properties.contains(.deviceModel) { result.model = "Isocast Virtual Camera" }
        return result
    }
    func setDeviceProperties(_ properties: CMIOExtensionDeviceProperties) throws {}

    func configure(index newIndex: Int?, duration newDuration: CMTime?, sink: Bool) throws {
        try negotiation.configure(index: newIndex, duration: newDuration, sink: sink, producing: producer != nil)
        if sink {
            input.stream.notifyPropertiesChanged(states(inputIndex, inputDuration))
        }
        updateOutput()
    }

    private func states(_ index: Int, _ duration: CMTime) -> [CMIOExtensionProperty: CMIOExtensionPropertyState<AnyObject>] {
        [.streamActiveFormatIndex: CMIOExtensionPropertyState(value: NSNumber(value: index)),
         .streamFrameDuration: CMIOExtensionPropertyState(value: CMTimeCopyAsDictionary(duration, allocator: nil)! as NSDictionary)]
    }

    private func updateOutput() {
        let (wantedIndex, wantedDuration) = negotiation.effective(producing: producer != nil)
        let changed = index != wantedIndex || CMTimeCompare(duration, wantedDuration) != 0
        index = wantedIndex; duration = wantedDuration
        if changed {
            card = nil
            if let timer { timer.schedule(deadline: .now(), repeating: duration.seconds, leeway: .milliseconds(1)) }
        }
        output.stream.notifyPropertiesChanged(states(index, duration))
    }

    func startOutput() {
        viewers += 1
        guard timer == nil else { return }
        let timer = DispatchSource.makeTimerSource(flags: .strict, queue: cameraQueue)
        timer.schedule(deadline: .now(), repeating: duration.seconds, leeway: .milliseconds(1))
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            let now = DispatchTime.now().uptimeNanoseconds
            let stale = UInt64(max(0.5, self.duration.seconds * 3) * 1e9)
            if self.lastArrival == 0 || now &- self.lastArrival > stale { self.sendCard() }
        }
        self.timer = timer; timer.resume()
    }

    func stopOutput() {
        viewers = max(0, viewers - 1)
        if viewers == 0 { timer?.cancel(); timer = nil; lastPTS = .invalid }
    }

    func startInput(_ client: CMIOExtensionClient) throws {
        guard producer == nil else { throw problem("Isocast camera already has a producer") }
        producer = client; generation &+= 1
        updateOutput()
        consume(client, generation)
    }

    func stopInput() { producer = nil; generation &+= 1; lastArrival = 0; updateOutput() }

    private func consume(_ client: CMIOExtensionClient, _ epoch: UInt64) {
        guard producer != nil && generation == epoch else { return }
        input.stream.consumeSampleBuffer(from: client) { [weak self] sample, sequence, discontinuity, more, error in
            cameraQueue.async {
                guard let self, self.producer != nil, self.generation == epoch else { return }
                if let sample, error == nil {
                    self.forward(sample, discontinuity)
                    self.input.stream.notifyScheduledOutputChanged(CMIOExtensionScheduledOutput(
                        sequenceNumber: sequence, hostTimeInNanoseconds: DispatchTime.now().uptimeNanoseconds))
                }
                // Do not recurse on CMIO's callback stack, and never busy-spin
                // an empty queue. Late frames are discarded by forward().
                let delay: DispatchTimeInterval = (more || sample != nil) ? .nanoseconds(0) : .nanoseconds(Int(self.inputDuration.seconds * 1e9))
                cameraQueue.asyncAfter(deadline: .now() + delay) { [weak self] in self?.consume(client, epoch) }
            }
        }
    }

    private func forward(_ sample: CMSampleBuffer, _ discontinuity: CMIOExtensionStream.DiscontinuityFlags) {
        guard let pixels = CMSampleBufferGetImageBuffer(sample), CVPixelBufferGetIOSurface(pixels) != nil,
              CVPixelBufferGetPixelFormatType(pixels) == kCVPixelFormatType_32BGRA,
              CVPixelBufferGetWidth(pixels) == sizes[index].0, CVPixelBufferGetHeight(pixels) == sizes[index].1 else { return }
        let pts = CMSampleBufferGetPresentationTimeStamp(sample)
        let now = CMClockGetTime(CMClockGetHostTimeClock())
        guard pts.isValid, CMTimeSubtract(now, pts).seconds < max(0.5, duration.seconds * 3),
              CMTimeSubtract(pts, now).seconds < 0.1 else { return }
        lastArrival = DispatchTime.now().uptimeNanoseconds
        guard viewers > 0 else { return }
        let presented = clock.next(pts, duration: duration, card: false)
        guard !lastPTS.isValid || CMTimeCompare(presented, lastPTS) > 0 else { return }
        var timing = CMSampleTimingInfo(duration: inputDuration, presentationTimeStamp: presented, decodeTimeStamp: .invalid)
        var timed: CMSampleBuffer?
        guard CMSampleBufferCreateCopyWithNewTiming(allocator: nil, sampleBuffer: sample,
            sampleTimingEntryCount: 1, sampleTimingArray: &timing, sampleBufferOut: &timed) == noErr, let timed else { return }
        // Timing-only copy; same IOSurface. Preserve capture PTS for diagnosis.
        CMSetAttachment(timed, key: "dev.isocast.program-pts-ns" as CFString,
                        value: NSNumber(value: pts.seconds * 1e9), attachmentMode: kCMAttachmentMode_ShouldPropagate)
        output.stream.send(timed, discontinuity: discontinuity,
                           hostTimeInNanoseconds: UInt64(presented.seconds * 1e9))
        lastPTS = presented
    }


    private func sendCard() {
        if card == nil { card = waitingCard(sizes[index].0, sizes[index].1) }
        guard let card else { return }
        var description: CMVideoFormatDescription?
        guard CMVideoFormatDescriptionCreateForImageBuffer(allocator: nil, imageBuffer: card,
            formatDescriptionOut: &description) == noErr, let description else { return }
        var timing = CMSampleTimingInfo(duration: duration,
            presentationTimeStamp: clock.next(CMClockGetTime(CMClockGetHostTimeClock()), duration: duration, card: true), decodeTimeStamp: .invalid)
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreateForImageBuffer(allocator: nil, imageBuffer: card, dataReady: true,
            makeDataReadyCallback: nil, refcon: nil, formatDescription: description,
            sampleTiming: &timing, sampleBufferOut: &sample) == noErr, let sample else { return }
        lastPTS = timing.presentationTimeStamp
        output.stream.send(sample, discontinuity: [], hostTimeInNanoseconds: UInt64(lastPTS.seconds * 1e9))
    }
}

// One immutable card per format; its pixels are never redrawn while a
// consumer holds them. BGRA, full-range RGB with Rec.709 primaries/transfer.
func waitingCard(_ width: Int, _ height: Int) -> CVPixelBuffer? {
    var pixels: CVPixelBuffer?
    let attributes: [CFString: Any] = [kCVPixelBufferIOSurfacePropertiesKey: [:]]
    guard CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA,
        attributes as CFDictionary, &pixels) == kCVReturnSuccess, let pixels else { return nil }
    guard CVPixelBufferLockBaseAddress(pixels, []) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pixels, []) }
    guard let context = CGContext(data: CVPixelBufferGetBaseAddress(pixels), width: width, height: height,
        bitsPerComponent: 8, bytesPerRow: CVPixelBufferGetBytesPerRow(pixels),
        space: CGColorSpace(name: CGColorSpace.itur_709)!,
        bitmapInfo: CGBitmapInfo.byteOrder32Little.rawValue | CGImageAlphaInfo.premultipliedFirst.rawValue) else { return nil }
    context.setFillColor(CGColor(gray: 0.06, alpha: 1))
    context.fill(CGRect(x: 0, y: 0, width: width, height: height))
    let scale = CGFloat(min(width, height)) / 1080
    for (text, pointSize, offset, gray) in [("Isocast", 72.0, 12.0, 0.92), ("Virtual Camera · Waiting for video", 24.0, -50.0, 0.55)] {
        let font = CTFontCreateWithName("HelveticaNeue" as CFString, pointSize * scale, nil)
        let attributes: [NSAttributedString.Key: Any] = [.init(kCTFontAttributeName as String): font,
            .init(kCTForegroundColorAttributeName as String): CGColor(gray: gray, alpha: 1)]
        let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attributes))
        let bounds = CTLineGetBoundsWithOptions(line, .useGlyphPathBounds)
        context.textPosition = CGPoint(x: (CGFloat(width)-bounds.width)/2, y: CGFloat(height)/2 + offset*scale)
        CTLineDraw(line, context)
    }
    tag(pixels)
    return pixels
}

final class CameraStream: NSObject, CMIOExtensionStreamSource {
    private(set) var stream: CMIOExtensionStream!
    unowned let owner: CameraDevice
    let direction: CMIOExtensionStream.Direction
    private var candidates: [UUID: CMIOExtensionClient] = [:]
    let formats: [CMIOExtensionStreamFormat] = sizes.map { w,h in
        CMIOExtensionStreamFormat(formatDescription: format(w,h), maxFrameDuration: CMTime(value: 1,timescale: 1),
                                  minFrameDuration: CMTime(value: 1,timescale: 240), validFrameDurations: nil)
    }
    init(_ owner: CameraDevice, _ direction: CMIOExtensionStream.Direction, _ id: String) {
        self.owner = owner; self.direction = direction
        super.init()
        stream = CMIOExtensionStream(localizedName: direction == .source ? "Isocast Virtual Camera" : "Isocast Camera Sink",
            streamID: UUID(uuidString: id)!, direction: direction, clockType: .hostTime, source: self)
    }
    var availableProperties: Set<CMIOExtensionProperty> {
        direction == .sink ? [.streamActiveFormatIndex, .streamFrameDuration, .streamSinkBufferQueueSize, .streamSinkBuffersRequiredForStartup] : [.streamActiveFormatIndex, .streamFrameDuration]
    }
    func streamProperties(forProperties properties: Set<CMIOExtensionProperty>) throws -> CMIOExtensionStreamProperties {
        let result = CMIOExtensionStreamProperties(dictionary: [:])
        if properties.contains(.streamActiveFormatIndex) { result.activeFormatIndex = direction == .sink ? owner.inputIndex : owner.index }
        if properties.contains(.streamFrameDuration) { result.frameDuration = direction == .sink ? owner.inputDuration : owner.duration }
        if properties.contains(.streamSinkBufferQueueSize) { result.sinkBufferQueueSize = 4 }
        if properties.contains(.streamSinkBuffersRequiredForStartup) { result.sinkBuffersRequiredForStartup = 1 }
        return result
    }
    func setStreamProperties(_ properties: CMIOExtensionStreamProperties) throws {
        try owner.configure(index: properties.activeFormatIndex, duration: properties.frameDuration, sink: direction == .sink)
    }
    func authorizedToStartStream(for client: CMIOExtensionClient) -> Bool {
        if direction == .sink {
            guard producerCodeAllowed(signingID: client.signingID, pid: client.pid) else { return false }
            guard owner.producer == nil, candidates.isEmpty || candidates[client.clientID] != nil else {
                log.error("refused producer pid \(client.pid, privacy: .public): another producer holds the sink")
                return false
            }
            candidates[client.clientID] = client
        }
        return true
    }
    func startStream() throws {
        if direction == .source { owner.startOutput() }
        else if candidates.count == 1, let candidate = candidates.values.first { try owner.startInput(candidate) }
        else { throw problem("No authorized camera producer") }
    }
    func stopStream() throws {
        if direction == .source { owner.stopOutput() } else { owner.stopInput(); candidates.removeAll() }
    }
    func disconnected(_ client: CMIOExtensionClient) {
        candidates.removeValue(forKey: client.clientID)
        if owner.producer?.clientID == client.clientID { owner.stopInput() }
    }
}

final class CameraProvider: NSObject, CMIOExtensionProviderSource {
    private(set) var provider: CMIOExtensionProvider!
    private let camera = CameraDevice()
    override init() {
        super.init()
        provider = CMIOExtensionProvider(source: self, clientQueue: cameraQueue)
        do { try provider.addDevice(camera.device) } catch { fatalError("Isocast camera: \(error)") }
    }
    func connect(to client: CMIOExtensionClient) throws {}
    func disconnect(from client: CMIOExtensionClient) {
        camera.input.disconnected(client)
    }
    var availableProperties: Set<CMIOExtensionProperty> { [.providerManufacturer] }
    func providerProperties(forProperties properties: Set<CMIOExtensionProperty>) throws -> CMIOExtensionProviderProperties {
        let result = CMIOExtensionProviderProperties(dictionary: [:])
        if properties.contains(.providerManufacturer) { result.manufacturer = "Isocast" }
        return result
    }
    func setProviderProperties(_ properties: CMIOExtensionProviderProperties) throws {}
}
