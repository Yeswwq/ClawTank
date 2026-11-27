import CoreVideo
import VideoToolbox
import XCTest
@testable import ClawTank

/// Builds IOSurface-backed NV12 frames with a moving bar so successive frames differ.
enum SyntheticFrames {
    static func make(width: Int, height: Int, index: Int) -> CVPixelBuffer {
        var buffer: CVPixelBuffer?
        let attributes: [CFString: Any] = [kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary]
        CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                            attributes as CFDictionary, &buffer)
        guard let buffer else { fatalError("CVPixelBufferCreate failed") }
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        let luma = CVPixelBufferGetBaseAddressOfPlane(buffer, 0)!.assumingMemoryBound(to: UInt8.self)
        let lumaStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let barX = (index * 24) % width
        for row in 0..<height {
            let line = luma + row * lumaStride
            for col in 0..<width {
                let inBar = col >= barX && col < barX + 64
                line[col] = inBar ? 235 : UInt8(16 + (col + row) % 200)
            }
        }
        let chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1)!.assumingMemoryBound(to: UInt8.self)
        let chromaStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)
        for row in 0..<(height / 2) {
            memset(chroma + row * chromaStride, 128, width)
        }
        return buffer
    }
}

/// Collects encoder output thread-safely.
final class FrameSink: @unchecked Sendable {
    private let condition = NSCondition()
    private var frames: [EncodedFrame] = []
    func append(_ frame: EncodedFrame) {
        condition.lock()
        frames.append(frame)
        condition.broadcast()
        condition.unlock()
    }

    var all: [EncodedFrame] {
        condition.lock()
        defer { condition.unlock() }
        return frames
    }

    func waitForCount(_ count: Int, timeout: TimeInterval = 2) -> Bool {
        let deadline = Date(timeIntervalSinceNow: timeout)
        condition.lock()
        defer { condition.unlock() }
        while frames.count < count {
            guard condition.wait(until: deadline) else { break }
        }
        return frames.count >= count
    }
}

/// NAL unit types in an Annex-B access unit (4-byte start codes, as the encoder emits).
func nalTypes(_ data: Data) -> [UInt8] {
    let bytes = [UInt8](data)
    var types: [UInt8] = []
    var index = 0
    while index + 4 < bytes.count {
        if bytes[index] == 0, bytes[index + 1] == 0, bytes[index + 2] == 0, bytes[index + 3] == 1 {
            types.append(bytes[index + 4] & 0x1f)
            index += 4
        } else {
            index += 1
        }
    }
    return types
}

final class H264EncoderTests: XCTestCase {
    private func makeEncoder(width: Int = 640, height: Int = 368, sink: FrameSink) throws -> H264Encoder {
        let settings = H264Encoder.Settings(width: width, height: height, averageBitRate: 4_000_000,
                                            requireHardware: false)
        return try H264Encoder(settings: settings) { sink.append($0) }
    }

    func testAlignment() {
        XCTAssertEqual(H264Encoder.Settings.aligned(1512), 1520)
        XCTAssertEqual(H264Encoder.Settings.aligned(982), 992)
        XCTAssertEqual(H264Encoder.Settings.aligned(1600), 1600)
    }

    func testOutputIsAnnexBWithParameterSetsOnKeyframes() throws {
        let sink = FrameSink()
        let encoder = try makeEncoder(sink: sink)
        for index in 0..<10 {
            encoder.encode(SyntheticFrames.make(width: 640, height: 368, index: index),
                           presentationTime: CMTime(value: CMTimeValue(index), timescale: 60),
                           forceKeyframe: index == 6)
            XCTAssertTrue(sink.waitForCount(index + 1), "encoder did not emit frame \(index)")
        }
        encoder.flush()
        let frames = sink.all
        XCTAssertEqual(frames.count, 10)

        let first = try XCTUnwrap(frames.first)
        XCTAssertTrue(first.isKeyframe, "stream must start with an IDR")
        XCTAssertEqual(Array(first.data.prefix(4)), [0, 0, 0, 1], "Annex-B start code")
        let firstTypes = nalTypes(first.data)
        XCTAssertEqual(Array(firstTypes.prefix(2)), [7, 8], "SPS then PPS before the IDR")
        XCTAssertTrue(firstTypes.contains(5), "IDR slice present")

        XCTAssertFalse(frames[1].isKeyframe, "no unsolicited IDR right after the first")
        XCTAssertFalse(nalTypes(frames[1].data).contains(7), "P-frames carry no SPS")

        XCTAssertTrue(frames[6].isKeyframe, "forced keyframe honoured")
        XCTAssertEqual(Array(nalTypes(frames[6].data).prefix(2)), [7, 8])
        XCTAssertTrue(frames.allSatisfy { $0.encodeDuration >= 0 })
    }

    func testRejectsUnalignedSizeAtTheBoundary() {
        XCTAssertEqual(H264Encoder.Settings.aligned(1) % 16, 0)
        XCTAssertGreaterThanOrEqual(
            H264Encoder.Settings.defaultBitRate(width: 640, height: 368, frameRate: 60), 2_000_000)
    }

    func testAVCCConversionRejectsTruncatedAccessUnits() {
        let truncatedNAL: [UInt8] = [0, 0, 0, 3, 0x65, 0x88]
        var output = Data()
        truncatedNAL.withUnsafeBufferPointer {
            XCTAssertFalse(H264Encoder.appendAVCCNALUnits(from: $0, headerLength: 4, to: &output))
        }
        XCTAssertTrue(output.isEmpty, "a rejected access unit must not expose a partial bitstream")

        let truncatedHeader: [UInt8] = [0, 0]
        output.removeAll(keepingCapacity: true)
        truncatedHeader.withUnsafeBufferPointer {
            XCTAssertFalse(H264Encoder.appendAVCCNALUnits(from: $0, headerLength: 4, to: &output))
        }
        XCTAssertTrue(output.isEmpty)
    }

    func testAVCCConversionAcceptsCompleteAccessUnit() {
        let avcc: [UInt8] = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x06]
        var output = Data()
        avcc.withUnsafeBufferPointer {
            XCTAssertTrue(H264Encoder.appendAVCCNALUnits(from: $0, headerLength: 4, to: &output))
        }
        XCTAssertEqual(Array(output), [0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x06])
    }

    func testKeyframeRecoveryStateKeepsPFramesAfterSuccessfulKeyframe() {
        var state = VideoSession.KeyframeRecoveryState()
        XCTAssertTrue(state.needsKeyframe)

        state.submission(frameIsKeyframe: true, result: .sent)
        XCTAssertFalse(state.needsKeyframe)

        state.submission(frameIsKeyframe: false, result: .sent)
        XCTAssertFalse(state.needsKeyframe, "successful P-frames preserve the active reference chain")

        state.encoderFailed()
        XCTAssertTrue(state.needsKeyframe)

        state.submission(frameIsKeyframe: false, result: .sent)
        XCTAssertTrue(state.needsKeyframe)
        state.submission(frameIsKeyframe: true, result: .needKeyframe)
        XCTAssertTrue(state.needsKeyframe)
        state.submission(frameIsKeyframe: true, result: .sent)
        XCTAssertFalse(state.needsKeyframe)

        state.submission(frameIsKeyframe: false, result: .notReady)
        XCTAssertFalse(state.needsKeyframe, "a deferred frame does not break the active reference chain")
        state.submission(frameIsKeyframe: true, result: .sent)
        XCTAssertFalse(state.needsKeyframe)
        state.clientResumed()
        XCTAssertTrue(state.needsKeyframe, "client refresh invalidates the old reference chain")
    }

    func testCachedFrameCanBeReplayedWithIncreasingPresentationTimes() {
        var cache = CachedVideoFrame()
        let buffer = SyntheticFrames.make(width: 640, height: 368, index: 0)
        let capturedTime = CMTime(value: 10, timescale: 60)
        XCTAssertEqual(cache.capture(buffer, at: capturedTime, frameRate: 60), capturedTime)

        let firstReplay = cache.replay(frameRate: 60)
        let secondReplay = cache.replay(frameRate: 60)

        XCTAssertNotNil(firstReplay?.0)
        XCTAssertEqual(firstReplay?.1, CMTime(value: 11, timescale: 60))
        XCTAssertEqual(secondReplay?.1, CMTime(value: 12, timescale: 60))
        cache.clear()
        XCTAssertNil(cache.replay(frameRate: 60), "stop releases the retained pixel buffer")
    }

    func testFrameGenerationDropsOlderPFramesAfterForcedKeyframe() {
        var generation = VideoFrameGeneration()
        let first = generation.submitted(keyframe: true)
        let staleP = generation.submitted(keyframe: false)
        let resumeIdr = generation.submitted(keyframe: true)

        XCTAssertEqual(first, 1)
        XCTAssertEqual(staleP, 1)
        XCTAssertEqual(resumeIdr, 2)
        XCTAssertFalse(generation.delivered(keyframe: false, generation: first),
                       "pre-IDR output is stale")
        XCTAssertTrue(generation.delivered(keyframe: true, generation: resumeIdr))
        XCTAssertTrue(generation.delivered(keyframe: false, generation: resumeIdr))
    }

    func testSessionCreationErrorIncludesVideoToolboxStatus() {
        let error = EncoderError.sessionCreation(kVTParameterErr)
        XCTAssertTrue(error.errorDescription?.contains("-12902") == true)
        XCTAssertTrue(error.errorDescription?.contains("kVTParameterErr") == true)
    }

    func testVideoToolboxMalfunctionRequiresSessionRebuild() {
        XCTAssertTrue(EncoderError.encodeFrame(kVTVideoEncoderMalfunctionErr).requiresSessionRebuild)
        XCTAssertTrue(EncoderError.encodeFrame(kVTInvalidSessionErr).requiresSessionRebuild)
        XCTAssertFalse(EncoderError.encodeFrame(kVTVideoEncoderNotAvailableNowErr).requiresSessionRebuild)
        XCTAssertFalse(EncoderError.annexBConversion.requiresSessionRebuild)
    }

    func testHardwareEncoderReportsSelectedImplementation() throws {
        let sink = FrameSink()
        let settings = H264Encoder.Settings(width: 640, height: 368, averageBitRate: 4_000_000,
                                            requireHardware: true, lowLatencyRateControl: true)
        let encoder: H264Encoder
        do {
            encoder = try H264Encoder(settings: settings) { sink.append($0) }
        } catch let error as EncoderError {
            guard case let .sessionCreation(status) = error,
                  status == kVTCouldNotFindVideoEncoderErr else { throw error }
            throw XCTSkip("This host has no hardware H.264 encoder")
        }

        XCTAssertNotEqual(encoder.encoderID, "unknown")
        if let hardware = encoder.isHardwareAccelerated {
            XCTAssertTrue(hardware)
        }
        if encoder.encoderID.contains("nvmtl") {
            XCTAssertFalse(encoder.lowLatencyRateControlEnabled,
                           "NVMTL currently rejects VideoToolbox low-latency rate control")
        }

        encoder.encode(SyntheticFrames.make(width: 640, height: 368, index: 0),
                       presentationTime: CMTime(value: 0, timescale: 60), forceKeyframe: true)
        encoder.flush()
        XCTAssertFalse(sink.all.isEmpty, "hardware encoder should emit a frame")
    }

    func testVideoStatsFormatting() {
        let text = ServerController.formatVideoStats(fps: 60, bytesPerSecond: 1_500_000, encodeMs: 3.14, skipped: 0)
        let expected = String(localized: """
            60 fps · \(12.0, format: .number.precision(.fractionLength(1))) Mbit/s · \
            encoding \(3.14, format: .number.precision(.fractionLength(1))) ms
            """)
        XCTAssertEqual(text, expected)
        let skipped = ServerController.formatVideoStats(fps: 1, bytesPerSecond: 0, encodeMs: 0, skipped: 2)
        XCTAssertTrue(skipped.contains("2"))
        let adaptive = ServerController.formatAdaptiveVideoStats(ackDelayMs: 12, targetBitrate: 2_500_000,
                                                                 acknowledgedGoodputBps: 1_200_000,
                                                                 congested: true)
        XCTAssertTrue(adaptive.contains("12"))
        XCTAssertTrue(adaptive.contains("1.2"))
        XCTAssertTrue(adaptive.contains("2.5"))
        XCTAssertTrue(adaptive.contains(String(localized: "Congested")))
        let pending = ServerController.formatAdaptiveVideoStats(ackDelayMs: 0, targetBitrate: 2_500_000,
                                                                congested: false)
        XCTAssertEqual(pending.components(separatedBy: String(localized: "pending")).count - 1, 2)
    }

    func testConnectionDiagnosticsFormattingNamesPendingAndSelectedFacts() {
        var state = RdpServer.RdpDiagnostics()
        state.width = 2316
        state.height = 1080
        state.scalePercent = 200
        state.deviceScalePercent = 180
        state.monitorCount = 1
        state.transport = "tcp"
        let pending = ServerController.formatDiagnosticsOverlay(state, connected: true)
        XCTAssertTrue(pending.contains("2316x1080"))
        XCTAssertTrue(pending.contains("200%"))
        XCTAssertTrue(state.selectedVideoPath == nil)
        XCTAssertTrue(state.clientCapabilitiesKnown == false)
        XCTAssertTrue(pending.contains("TCP"))

        state.clientCapabilitiesKnown = true
        state.clientSupportsUDPFECR = true
        state.clientSupportsSoftSync = true
        state.multitransportOffered = true
        state.multitransportResponse = true
        state.egfxOpen = true
        state.egfxChannelID = 7
        state.egfxCapabilityVersion = 0x0a04_0000
        state.selectedVideoPath = "RemoteFXProgressive"
        state.legacyImageRemoteFX = true
        state.legacyNSCodec = true
        state.legacySelectedCodec = "NSCodec"
        state.clientEarlyCapabilityFlags = 0x190
        state.clientInfoFlags = 0x213_435
        state.performanceFlags = 6
        state.clientQueueDepth = 3
        state.clientFramesDecoded = 42
        state.clientDecodeRenderMilliseconds = 17
        state.clientQoEReports = 2
        state.networkRTTMilliseconds = 18
        state.networkBaselineRTTMilliseconds = 14
        state.networkBandwidthKilobitsPerSecond = 125_000
        state.transport = "udp"
        let selected = ServerController.formatDiagnosticsOverlay(state, connected: true)
        XCTAssertTrue(selected.contains("RemoteFXProgressive"))
        XCTAssertTrue(selected.contains("NSCodec"))
        XCTAssertTrue(selected.contains("UDP"))
        let selectedLower = selected.lowercased()
        XCTAssertTrue(selectedLower.contains("supported") || selected.contains("支持"))
        XCTAssertTrue(selectedLower.contains("accepted") || selected.contains("已接受"))
        XCTAssertTrue(selected.contains("42"))
        XCTAssertTrue(selected.contains("17 ms"))
        XCTAssertTrue(selected.contains("18"))
        XCTAssertTrue(selected.contains("0.1"))
        XCTAssertTrue(selected.contains("AutoDetect"))
        XCTAssertFalse(selected.contains("Client formats: none advertised"))
    }
}

/// `make bench`: encode 2560×1600 frames with motion at 60 fps and report latency, bitrate
/// and fps for each encoder configuration. Skipped unless CLAWTANK_BENCH=1.
final class EncoderBenchmarkTests: XCTestCase {
    func testEncode2560x1600() throws {
        try XCTSkipUnless(ProcessInfo.processInfo.environment["CLAWTANK_BENCH"] == "1", "set CLAWTANK_BENCH=1")
        let (width, height) = (2560, 1600)
        let frames = (0..<30).map { SyntheticFrames.make(width: width, height: height, index: $0 * 7) }
        var lines: [String] = []
        for lowLatency in [false, true] {
            let settings = H264Encoder.Settings(
                width: width, height: height,
                averageBitRate: H264Encoder.Settings.defaultBitRate(width: width, height: height, frameRate: 60),
                requireHardware: true, lowLatencyRateControl: lowLatency)
            lines.append(try Self.run(settings, frames: frames, count: 300))
        }
        let report = lines.joined(separator: "\n")
        print(report)
        try report.write(toFile: NSTemporaryDirectory() + "clawtank-bench.txt", atomically: true, encoding: .utf8)
    }

    private static func run(_ settings: H264Encoder.Settings, frames: [CVPixelBuffer], count: Int) throws -> String {
        let sink = FrameSink()
        let encoder = try H264Encoder(settings: settings) { sink.append($0) }
        let started = CFAbsoluteTimeGetCurrent()
        for index in 0..<count {
            encoder.encode(frames[index % frames.count],
                           presentationTime: CMTime(value: CMTimeValue(index), timescale: 60),
                           forceKeyframe: false)
            // Pace at 60 fps like real capture, so latency is per-frame, not queueing.
            let wait = started + Double(index + 1) / 60 - CFAbsoluteTimeGetCurrent()
            if wait > 0 { Thread.sleep(forTimeInterval: wait) }
        }
        encoder.flush()
        let elapsed = CFAbsoluteTimeGetCurrent() - started
        let out = sink.all
        XCTAssertEqual(out.count, count)
        let latencies = out.map { $0.encodeDuration * 1000 }.sorted()
        let bytes = out.reduce(0) { $0 + $1.data.count }
        return String(
            format: "BENCH %dx%d lowLatency=%@ encoder=%@ frames=%d fps=%.1f p50=%.2fms p95=%.2fms %.1fMbit/s",
            settings.width, settings.height, settings.lowLatencyRateControl ? "yes" : "no",
            encoder.encoderID.replacingOccurrences(of: "com.apple.videotoolbox.videoencoder.", with: ""),
            out.count, Double(out.count) / elapsed, latencies[latencies.count / 2],
            latencies[Int(Double(latencies.count) * 0.95)], Double(bytes * 8) / elapsed / 1_000_000)
    }
}
