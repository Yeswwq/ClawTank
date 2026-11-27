// Hardware H.264 encoder (VideoToolbox) tuned for remote-desktop latency: request low-latency
// rate control when supported, no frame reordering (no B-frames), and on-demand IDRs. Output is the
// Annex-B byte stream MS-RDPEGFX requires (RFX_AVC420_BITMAP_STREAM), with SPS/PPS prepended to
// every keyframe so a client can join or recover at any IDR.

import CoreMedia
import Foundation
import os
import VideoToolbox

struct EncodedFrame: Sendable {
    /// Annex-B access unit.
    let data: Data
    let isKeyframe: Bool
    /// Wall time from `encode` to the output callback.
    let encodeDuration: TimeInterval
    /// Reference-chain generation at submission. Outputs from an older
    /// generation are obsolete after a forced keyframe.
    let referenceGeneration: UInt64
}

enum EncoderError: Error, Equatable, LocalizedError, Sendable {
    case sessionCreation(OSStatus)
    case prepare(OSStatus)
    case property(String, OSStatus)
    case encodeFrame(OSStatus)
    case annexBConversion

    var requiresSessionRebuild: Bool {
        guard case let .encodeFrame(status) = self else { return false }
        return status == kVTVideoEncoderMalfunctionErr || status == kVTInvalidSessionErr
    }

    var errorDescription: String? {
        switch self {
        case let .sessionCreation(status):
            return "VideoToolbox session creation failed: \(Self.statusDescription(status))"
        case let .prepare(status):
            return "VideoToolbox encoder preparation failed: \(Self.statusDescription(status))"
        case let .property(key, status):
            return "VideoToolbox property \(key) failed: \(Self.statusDescription(status))"
        case let .encodeFrame(status):
            return "VideoToolbox frame encoding failed: \(Self.statusDescription(status))"
        case .annexBConversion:
            return "VideoToolbox output contained an invalid or truncated AVCC access unit"
        }
    }

    private static func statusDescription(_ status: OSStatus) -> String {
        let name: String? = switch status {
        case noErr: "noErr"
        case kVTPropertyNotSupportedErr: "kVTPropertyNotSupportedErr"
        case kVTPropertyReadOnlyErr: "kVTPropertyReadOnlyErr"
        case kVTParameterErr: "kVTParameterErr"
        case kVTInvalidSessionErr: "kVTInvalidSessionErr"
        case kVTAllocationFailedErr: "kVTAllocationFailedErr"
        case kVTCouldNotFindVideoEncoderErr: "kVTCouldNotFindVideoEncoderErr"
        case kVTVideoEncoderMalfunctionErr: "kVTVideoEncoderMalfunctionErr"
        case kVTVideoEncoderNotAvailableNowErr: "kVTVideoEncoderNotAvailableNowErr"
        default: nil
        }
        let raw = UInt32(bitPattern: status)
        let hex = String(format: "0x%08X", raw)
        return "OSStatus \(status) (\(hex))"
            + (name.map { " [\($0)]" } ?? "")
    }
}

final class H264Encoder: @unchecked Sendable {
    struct Settings: Equatable {
        /// Coded size. MS-RDPEGFX requires multiples of 16.
        var width: Int
        var height: Int
        var frameRate: Int = 60
        var averageBitRate: Int
        /// Seconds between unsolicited IDRs (recovery for clients that missed one).
        var keyframeIntervalSeconds: Double = 10
        /// Production requires the hardware encoder; tests on VMs may allow software.
        var requireHardware = true
        /// Apple's low-latency rate control (selects the real-time "rtvc" encoder).
        var lowLatencyRateControl = false

        static func aligned(_ value: Int) -> Int { (value + 15) / 16 * 16 }

        /// Bit rate scaled from a 2560×1600 @ 60 fps baseline.
        static func defaultBitRate(width: Int, height: Int, frameRate: Int) -> Int {
            let pixelsPerSecond = Double(width * height * frameRate)
            let baseline = Double(2560 * 1600 * 60)
            return max(2_000_000, Int(20_000_000 * pixelsPerSecond / baseline))
        }
    }

    let settings: Settings
    private let session: VTCompressionSession
    /// Whether the encoder accepted the low-latency specification. Some third-party
    /// VideoToolbox encoders reject that specification while still providing hardware H.264.
    let lowLatencyRateControlEnabled: Bool
    private let output: @Sendable (EncodedFrame) -> Void
    private let onFailure: @Sendable (EncoderError) -> Void
    private let log = Logger(subsystem: "dev.clawtank.ClawTank", category: "encode")
    private let invalidationLock = NSLock()
    private var invalidated = false

    private struct SessionCreation {
        let session: VTCompressionSession
        let lowLatencyEnabled: Bool
        let fallbackStatus: OSStatus?
    }

    init(settings: Settings, output: @escaping @Sendable (EncodedFrame) -> Void,
         onFailure: @escaping @Sendable (EncoderError) -> Void = { _ in }) throws {
        precondition(settings.width % 16 == 0 && settings.height % 16 == 0, "coded size must be 16-aligned")
        self.settings = settings
        self.output = output
        self.onFailure = onFailure
        let creationMessage = """
            Creating H.264 encoder size=\(settings.width)x\(settings.height) \
            fps=\(settings.frameRate) bitrate=\(settings.averageBitRate) \
            requireHardware=\(settings.requireHardware) lowLatencyRequested=\(settings.lowLatencyRateControl)
            """
        log.info("\(creationMessage, privacy: .public)")

        let created = try Self.createSession(settings: settings)
        session = created.session
        lowLatencyRateControlEnabled = created.lowLatencyEnabled
        if let status = created.fallbackStatus {
            log.notice("VideoToolbox rejected low-latency rate control (\(status)); retrying standard hardware encoder")
        }

        let identifier = Self.copyProperty(kVTCompressionPropertyKey_EncoderID, in: session) as? String ?? "unknown"
        let hardware = Self.copyProperty(kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, in: session)
            as? Bool
        let hardwareText = hardware.map(String.init) ?? "unknown"
        log.info("VideoToolbox encoder created id=\(identifier, privacy: .public)")
        log.info("VideoToolbox encoder hardware=\(hardwareText, privacy: .public)")
        log.info("VideoToolbox encoder lowLatency=\(created.lowLatencyEnabled)")

        try configure(session: session, settings: settings)
    }

    private static func createSession(settings: Settings) throws -> SessionCreation {
        var spec: [CFString: Any] = [:]
        if settings.lowLatencyRateControl {
            spec[kVTVideoEncoderSpecification_EnableLowLatencyRateControl] = true
        }
        if settings.requireHardware {
            spec[kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder] = true
        }
        var created: VTCompressionSession?
        var status = VTCompressionSessionCreate(
            allocator: nil, width: Int32(settings.width), height: Int32(settings.height),
            codecType: kCMVideoCodecType_H264, encoderSpecification: spec as CFDictionary,
            imageBufferAttributes: nil, compressedDataAllocator: nil, outputCallback: nil,
            refcon: nil, compressionSessionOut: &created)
        guard status == kVTParameterErr, settings.lowLatencyRateControl else {
            guard status == noErr, let created else { throw EncoderError.sessionCreation(status) }
            return SessionCreation(session: created, lowLatencyEnabled: settings.lowLatencyRateControl,
                                   fallbackStatus: nil)
        }
        spec.removeValue(forKey: kVTVideoEncoderSpecification_EnableLowLatencyRateControl)
        created = nil
        status = VTCompressionSessionCreate(
            allocator: nil, width: Int32(settings.width), height: Int32(settings.height),
            codecType: kCMVideoCodecType_H264, encoderSpecification: spec as CFDictionary,
            imageBufferAttributes: nil, compressedDataAllocator: nil, outputCallback: nil,
            refcon: nil, compressionSessionOut: &created)
        guard status == noErr, let created else { throw EncoderError.sessionCreation(status) }
        return SessionCreation(session: created, lowLatencyEnabled: false, fallbackStatus: kVTParameterErr)
    }

    private func configure(session: VTCompressionSession, settings: Settings) throws {

        let keyframeFrames = Int(Double(settings.frameRate) * settings.keyframeIntervalSeconds)
        let properties: [(CFString, Any)] = [
            (kVTCompressionPropertyKey_RealTime, true),
            (kVTCompressionPropertyKey_AllowFrameReordering, false),
            (kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Main_AutoLevel),
            (kVTCompressionPropertyKey_H264EntropyMode, kVTH264EntropyMode_CABAC),
            (kVTCompressionPropertyKey_ExpectedFrameRate, settings.frameRate),
            (kVTCompressionPropertyKey_MaxKeyFrameInterval, keyframeFrames),
            (kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, settings.keyframeIntervalSeconds),
            (kVTCompressionPropertyKey_AverageBitRate, settings.averageBitRate),
            // Cap bursts at 1.5× the average over one second (bytes per second, seconds).
            (kVTCompressionPropertyKey_DataRateLimits, [settings.averageBitRate * 3 / 16, 1] as CFArray),
            (kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, true),
            (kVTCompressionPropertyKey_MaximizePowerEfficiency, false)
        ]
        for (key, value) in properties {
            let status = VTSessionSetProperty(session, key: key, value: value as CFTypeRef)
            // Some encoders reject optional tuning keys; only core ones are fatal.
            if status != noErr {
                let fatal = [kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_AllowFrameReordering,
                             kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_AverageBitRate]
                if fatal.contains(where: { $0 == key }) {
                    VTCompressionSessionInvalidate(session)
                    throw EncoderError.property(key as String, status)
                }
                log.notice("Encoder ignored \(key as String, privacy: .public): \(status)")
            }
        }
        let prepareStatus = VTCompressionSessionPrepareToEncodeFrames(session)
        guard prepareStatus == noErr else {
            VTCompressionSessionInvalidate(session)
            throw EncoderError.prepare(prepareStatus)
        }
    }

    deinit {
        invalidate()
    }

    func invalidate() {
        let shouldInvalidate = invalidationLock.withLock { () -> Bool in
            guard !invalidated else { return false }
            invalidated = true
            return true
        }
        guard shouldInvalidate else { return }
        VTCompressionSessionInvalidate(session)
    }

    /// VideoToolbox's identifier for the chosen encoder, e.g. "com.apple.videotoolbox.videoencoder.ave.avc".
    var encoderID: String {
        copyProperty(kVTCompressionPropertyKey_EncoderID) as? String ?? "unknown"
    }

    /// The runtime VideoToolbox result; nil means the encoder did not report this property.
    var isHardwareAccelerated: Bool? {
        copyProperty(kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder) as? Bool
    }

    private func copyProperty(_ key: CFString) -> CFTypeRef? {
        Self.copyProperty(key, in: session)
    }

    private static func copyProperty(_ key: CFString, in session: VTCompressionSession) -> CFTypeRef? {
        let value = UnsafeMutablePointer<CFTypeRef?>.allocate(capacity: 1)
        value.initialize(to: nil)
        defer {
            value.deinitialize(count: 1)
            value.deallocate()
        }
        let status = VTSessionCopyProperty(session, key: key, allocator: nil, valueOut: value)
        return status == noErr ? value.pointee : nil
    }

    /// Encodes one frame asynchronously; `output` is called on a VideoToolbox thread.
    func encode(_ pixelBuffer: CVPixelBuffer, presentationTime: CMTime, forceKeyframe: Bool,
                referenceGeneration: UInt64 = 0) {
        let started = CFAbsoluteTimeGetCurrent()
        if forceKeyframe {
            log.debug("Submitting forced H.264 keyframe pts=\(presentationTime.seconds)")
        }
        let options: CFDictionary? = forceKeyframe
            ? [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary : nil
        let status = VTCompressionSessionEncodeFrame(
            session, imageBuffer: pixelBuffer, presentationTimeStamp: presentationTime,
            duration: .invalid, frameProperties: options, infoFlagsOut: nil
        ) { [output, onFailure, log] status, _, sampleBuffer in
            guard status == noErr, let sampleBuffer else {
                log.error("Encode failed: \(Self.statusDescription(status), privacy: .public)")
                onFailure(.encodeFrame(status))
                return
            }
            guard let frame = Self.annexB(from: sampleBuffer) else {
                log.error("Encode output conversion to Annex-B failed")
                onFailure(.annexBConversion)
                return
            }
            if frame.isKeyframe {
                let durationMs = String(format: "%.2f", (CFAbsoluteTimeGetCurrent() - started) * 1000)
                log.info("Encoded H.264 keyframe bytes=\(frame.data.count) durationMs=\(durationMs)")
            }
            output(EncodedFrame(data: frame.data, isKeyframe: frame.isKeyframe,
                                encodeDuration: CFAbsoluteTimeGetCurrent() - started,
                                referenceGeneration: referenceGeneration))
        }
        if status != noErr {
            log.error("VTCompressionSessionEncodeFrame failed: \(Self.statusDescription(status), privacy: .public)")
            onFailure(.encodeFrame(status))
        }
    }

    private static func statusDescription(_ status: OSStatus) -> String {
        String(format: "OSStatus %d (0x%08X)", status, UInt32(bitPattern: status))
    }

    /// Changes the bit rate live (adaptive bitrate); takes effect within a few frames.
    func setBitRate(_ bitsPerSecond: Int) {
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate,
                             value: bitsPerSecond as CFTypeRef)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_DataRateLimits,
                             value: [bitsPerSecond * 3 / 16, 1] as CFArray)
    }

    /// Blocks until all submitted frames have been emitted.
    func flush() {
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
    }

}

extension H264Encoder {
    // MARK: - AVCC → Annex-B

    private static let startCode: [UInt8] = [0, 0, 0, 1]

    static func isKeyframe(_ sampleBuffer: CMSampleBuffer) -> Bool {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
                as? [[CFString: Any]], let first = attachments.first else { return true }
        return !(first[kCMSampleAttachmentKey_NotSync] as? Bool ?? false)
    }

    /// Converts a VideoToolbox sample (AVCC, length-prefixed NAL units) to Annex-B, prepending
    /// SPS/PPS from the format description on keyframes.
    static func annexB(from sampleBuffer: CMSampleBuffer) -> (data: Data, isKeyframe: Bool)? {
        guard let block = CMSampleBufferGetDataBuffer(sampleBuffer),
              let format = CMSampleBufferGetFormatDescription(sampleBuffer) else { return nil }
        let keyframe = isKeyframe(sampleBuffer)
        var out = Data()

        let nalHeaderLength = appendParameterSets(of: format, to: &out, include: keyframe)

        // VideoToolbox output is normally one contiguous block; copy (compressed bytes, small)
        // only if it isn't.
        var contiguous = block
        if !CMBlockBufferIsRangeContiguous(block, atOffset: 0, length: 0) {
            var copy: CMBlockBuffer?
            guard CMBlockBufferCreateContiguous(
                allocator: nil, sourceBuffer: block, blockAllocator: nil, customBlockSource: nil,
                offsetToData: 0, dataLength: 0, flags: 0, blockBufferOut: &copy) == kCMBlockBufferNoErr,
                let copy else { return nil }
            contiguous = copy
        }
        var totalLength = 0
        var dataPointer: UnsafeMutablePointer<CChar>?
        guard CMBlockBufferGetDataPointer(
            contiguous, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &totalLength,
            dataPointerOut: &dataPointer) == kCMBlockBufferNoErr, let dataPointer else { return nil }
        let headerLength = nalHeaderLength
        let converted = dataPointer.withMemoryRebound(to: UInt8.self, capacity: totalLength) { bytes in
            appendAVCCNALUnits(
                from: UnsafeBufferPointer(start: bytes, count: totalLength),
                headerLength: headerLength,
                to: &out)
        }
        guard converted else { return nil }
        return (out, keyframe)
    }

    /// Appends SPS/PPS (with start codes) when `include` is set; returns the AVCC NAL length size.
    private static func appendParameterSets(of format: CMFormatDescription, to out: inout Data,
                                            include: Bool) -> Int {
        var count = 0
        var headerLength: Int32 = 4
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format, parameterSetIndex: 0, parameterSetPointerOut: nil, parameterSetSizeOut: nil,
            parameterSetCountOut: &count, nalUnitHeaderLengthOut: &headerLength)
        guard include else { return Int(headerLength) }
        for index in 0..<count {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, parameterSetIndex: index, parameterSetPointerOut: &pointer,
                parameterSetSizeOut: &size, parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil)
            if status == noErr, let pointer {
                out.append(contentsOf: startCode)
                out.append(pointer, count: size)
            }
        }
        return Int(headerLength)
    }

    /// Appends AVCC NALs only when the entire access unit has valid length prefixes and payloads.
    @discardableResult
    static func appendAVCCNALUnits(from bytes: UnsafeBufferPointer<UInt8>, headerLength: Int,
                                   to output: inout Data) -> Bool {
        guard (1...4).contains(headerLength), !bytes.isEmpty, bytes.baseAddress != nil else { return false }

        var offset = 0
        var nalCount = 0
        while offset < bytes.count {
            guard bytes.count - offset >= headerLength else { return false }
            var nalLength = 0
            for index in offset..<(offset + headerLength) {
                nalLength = (nalLength << 8) | Int(bytes[index])
            }
            offset += headerLength
            guard nalLength > 0, nalLength <= bytes.count - offset else { return false }
            offset += nalLength
            nalCount += 1
        }
        guard nalCount > 0 else { return false }

        offset = 0
        while offset < bytes.count {
            var nalLength = 0
            for index in offset..<(offset + headerLength) {
                nalLength = (nalLength << 8) | Int(bytes[index])
            }
            offset += headerLength
            output.append(contentsOf: [0, 0, 0, 1])
            output.append(bytes.baseAddress!.advanced(by: offset), count: nalLength)
            offset += nalLength
        }
        return true
    }
}
