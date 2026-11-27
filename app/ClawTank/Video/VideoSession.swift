// One client's video stream: ScreenCapture → (optional StatsOverlay) → H264Encoder → core.
//
// Backpressure is applied *before* encoding: if the core says a frame would not be sent, the
// captured frame is skipped (nothing was encoded, so the reference chain is intact). If the
// core ever refuses an encoded frame, the next frame is forced to be an IDR.

import CoreMedia
import CoreVideo
import Dispatch
import Foundation
import os

private struct SendablePixelBuffer: @unchecked Sendable {
    let value: CVPixelBuffer
}

/// Output generation for the access-unit reference chain.
///
/// A forced IDR makes older queued P-frames redundant and, if drained after it,
/// invalid. Tagging each submission lets the delivery queue discard those
/// stale outputs without blocking VideoToolbox's callback thread.
struct VideoFrameGeneration: Equatable, Sendable {
    private(set) var value: UInt64 = 0

    mutating func submitted(keyframe: Bool) -> UInt64 {
        if keyframe {
            value &+= 1
        }
        return value
    }

    func delivered(keyframe _: Bool, generation: UInt64) -> Bool {
        generation == value
    }
}

struct CachedVideoFrame {
    private(set) var pixelBuffer: CVPixelBuffer?
    private(set) var presentationTime = CMTime.invalid

    mutating func capture(_ buffer: CVPixelBuffer, at time: CMTime, frameRate: Int) -> CMTime {
        let step = CMTime(value: 1, timescale: CMTimeScale(max(1, frameRate)))
        let nextAllowed = presentationTime.isValid ? CMTimeAdd(presentationTime, step) : time
        let timestamp = presentationTime.isValid && CMTimeCompare(time, nextAllowed) < 0 ? nextAllowed : time
        pixelBuffer = buffer
        presentationTime = timestamp
        return timestamp
    }

    mutating func replay(frameRate: Int) -> (CVPixelBuffer, CMTime)? {
        guard let pixelBuffer, presentationTime.isValid else { return nil }
        let nextTime = CMTimeAdd(presentationTime,
                                 CMTime(value: 1, timescale: CMTimeScale(max(1, frameRate))))
        presentationTime = nextTime
        return (pixelBuffer, nextTime)
    }

    mutating func clear() {
        pixelBuffer = nil
        presentationTime = .invalid
    }
}

final class VideoSession: @unchecked Sendable {
    struct AdaptiveState {
        let bitRate: Int
        let congested: Bool
        let acknowledgedGoodputBps: Double
    }

    struct KeyframeRecoveryState: Equatable {
        private(set) var needsKeyframe = true

        mutating func encoderFailed() {
            needsKeyframe = true
        }

        mutating func clientResumed() {
            needsKeyframe = true
        }

        mutating func submission(frameIsKeyframe: Bool, result: RdpServer.SubmitResult) {
            switch result {
            case .sent where frameIsKeyframe:
                needsKeyframe = false
            case .sent:
                break
            case .needKeyframe:
                needsKeyframe = true
            case .notReady:
                // No access unit was accepted. Keep the current reference-chain
                // state; the EGFX channel may simply be between capability
                // negotiation and DVC channel assignment.
                break
            }
        }
    }

    /// Monotonic counters; the controller turns deltas into per-second rates.
    struct Counters: Equatable {
        var capturedFrames = 0
        var skippedFrames = 0
        var encodedFrames = 0
        var sentFrames = 0
        var keyframes = 0
        var sentBytes = 0
        var encodeSeconds = 0.0
    }

    let contentWidth: Int
    let contentHeight: Int
    private let server: RdpServer
    private let overlayEnabled: @Sendable () -> Bool
    private let overlayState: AtomicOverlayState?
    private let onFirstFrameSent: (@Sendable () -> Void)?
    private let lock = NSLock()
    private let encodeQueue = DispatchQueue(label: "dev.clawtank.video.encode", qos: .userInteractive)
    private var counters = Counters()
    private var keyframeState = KeyframeRecoveryState()
    private var legacyOverlayText = ""
    private var encoder: H264Encoder?
    private var capture: ScreenCapture?
    private let overlay = StatsOverlay()
    private var adaptive: AdaptiveBitrate?
    private let sessionID = String(UUID().uuidString.prefix(8))
    private var deliveredFrames = 0
    private var lastReportedFirstFrame = false
    private var lastSubmitResult: RdpServer.SubmitResult?
    private var cachedFrame = CachedVideoFrame()
    private var stopped = false
    private var encoderSettings: H264Encoder.Settings?
    private var encoderGeneration: UInt64 = 0
    private var frameGeneration = VideoFrameGeneration()
    private var encoderRecoveryAttempts = 0
    private var successfulFramesSinceRecovery = 0
    private var encoderRestartInFlight = false
    private var resumeKeyframePending = false
    private var resumeKeyframeInFlight = false
    private var resumeKeyframeAttempts = 0
    private var resumeKeyframeAttemptID: UInt64 = 0
    private let log = Logger(subsystem: "dev.clawtank.ClawTank", category: "video")

    let target: DisplayTarget

    let frameRate: Int
    let bitRateFactor: Double

    init(server: RdpServer, width: Int, height: Int, target: DisplayTarget, frameRate: Int = 60,
         bitRateFactor: Double = 1, overlayEnabled: @escaping @Sendable () -> Bool,
         overlayState: AtomicOverlayState? = nil,
         onFirstFrameSent: (@Sendable () -> Void)? = nil) {
        self.server = server
        self.target = target
        self.frameRate = frameRate
        self.bitRateFactor = bitRateFactor
        self.contentWidth = width
        self.contentHeight = height
        self.overlayEnabled = overlayEnabled
        self.overlayState = overlayState
        self.onFirstFrameSent = onFirstFrameSent
    }

    var codedWidth: Int { H264Encoder.Settings.aligned(contentWidth) }
    var codedHeight: Int { H264Encoder.Settings.aligned(contentHeight) }

    var isHardwareAccelerated: Bool? { encoder?.isHardwareAccelerated }

    func snapshot() -> Counters {
        lock.withLock { counters }
    }

    /// Text drawn by the overlay (set by the controller once per second).
    func setOverlayText(_ text: String) {
        if let overlayState {
            overlayState.set(text: text)
        } else {
            lock.withLock { legacyOverlayText = text }
        }
    }

    /// Feeds one second of network measurements; returns the bit rate in use and whether the
    /// link looked congested.
    func adapt(_ sample: AdaptiveBitrate.Sample)
        -> AdaptiveState? {
        let (changed, state) = lock.withLock { () -> (Int?, AdaptiveBitrate?) in
            let changed = adaptive?.update(sample)
            return (changed, adaptive)
        }
        if let changed {
            encodeQueue.async { [weak self] in self?.encoder?.setBitRate(changed) }
        }
        return state.map {
            AdaptiveState(bitRate: $0.current, congested: $0.congested,
                          acknowledgedGoodputBps: $0.acknowledgedGoodputBps)
        }
    }
}

extension VideoSession {
    func start() async throws {
        let baseRate = H264Encoder.Settings.defaultBitRate(width: codedWidth, height: codedHeight, frameRate: frameRate)
        let settings = H264Encoder.Settings(
            width: codedWidth, height: codedHeight, frameRate: frameRate,
            averageBitRate: max(1_000_000, Int(Double(baseRate) * bitRateFactor)),
            lowLatencyRateControl: true)  // ADR-016: tighter p95 encode latency
        let startMessage = """
            Video session \(self.sessionID) starting \
            content=\(self.contentWidth)x\(self.contentHeight) \
            coded=\(self.codedWidth)x\(self.codedHeight) fps=\(self.frameRate) bitrate=\(settings.averageBitRate)
            """
        log.info("\(startMessage, privacy: .public)")
        lock.withLock { adaptive = AdaptiveBitrate(target: settings.averageBitRate) }
        let encoder = try configureEncoder(settings)

        let capture = ScreenCapture(
            onFrame: { [weak self] buffer, time in self?.handle(buffer, time) },
            onStop: { [weak self] _ in self?.log.notice("Capture ended") })
        self.capture = capture
        do {
            try await startCapture(capture)
        } catch {
            if self.capture === capture { self.capture = nil }
            await capture.stop()
            encodeQueue.sync {
                stopped = true
                encoder.flush()
                self.encoder = nil
                encoderSettings = nil
                encoderGeneration &+= 1
                encoderRestartInFlight = false
                cachedFrame.clear()
            }
            let errorMessage = "Video session \(self.sessionID) capture start failed: \(error.localizedDescription)"
            log.error("\(errorMessage, privacy: .public)")
            throw error
        }
        log.info("Video session \(self.sessionID, privacy: .public) capture and encoding started")
    }

    private func configureEncoder(_ settings: H264Encoder.Settings) throws -> H264Encoder {
        let generation = encodeQueue.sync {
            encoderGeneration &+= 1
            encoderSettings = settings
            encoderRecoveryAttempts = 0
            successfulFramesSinceRecovery = 0
            encoderRestartInFlight = false
            return encoderGeneration
        }
        let encoder = try makeEncoder(settings, generation: generation)
        encodeQueue.sync {
            self.encoder = encoder
            self.stopped = false
            self.cachedFrame.clear()
            self.resumeKeyframePending = false
            self.resumeKeyframeInFlight = false
            self.resumeKeyframeAttemptID &+= 1
        }
        let hardware = encoder.isHardwareAccelerated.map(String.init) ?? "unknown"
        let encoderMessage = """
            Video session \(self.sessionID) encoder id=\(encoder.encoderID) \
            hardware=\(hardware) lowLatency=\(encoder.lowLatencyRateControlEnabled)
            """
        log.info("\(encoderMessage, privacy: .public)")
        return encoder
    }

    private func makeEncoder(_ settings: H264Encoder.Settings, generation: UInt64) throws -> H264Encoder {
        do {
            return try H264Encoder(
                settings: settings,
                output: { [weak self] frame in self?.deliver(frame, generation: generation) },
                onFailure: { [weak self] error in self?.encoderFailed(error, generation: generation) })
        } catch {
            let message = "Video session \(self.sessionID) encoder creation failed: \(error.localizedDescription)"
            log.error("\(message, privacy: .public)")
            throw error
        }
    }

    private func startCapture(_ capture: ScreenCapture) async throws {
        try await capture.start(.init(
            contentWidth: contentWidth, contentHeight: contentHeight,
            codedWidth: codedWidth, codedHeight: codedHeight, frameRate: frameRate,
            displaySize: target.bounds.size), displayID: target.displayID)
    }

    func stop() async {
        await capture?.stop()
        capture = nil
        encodeQueue.sync {
            stopped = true
            encoder?.flush()
            encoder = nil
            encoderSettings = nil
            encoderGeneration &+= 1
            frameGeneration = VideoFrameGeneration()
            deliveredFrames = 0
            lastReportedFirstFrame = false
            encoderRestartInFlight = false
            cachedFrame.clear()
            resumeKeyframePending = false
            resumeKeyframeInFlight = false
            resumeKeyframeAttemptID &+= 1
        }
        let summary = lock.withLock { counters }
        let encodeSeconds = String(format: "%.3f", summary.encodeSeconds)
        let stoppedMessage = """
            Video session \(self.sessionID) stopped \
            captured=\(summary.capturedFrames) skipped=\(summary.skippedFrames) \
            encoded=\(summary.encodedFrames) keyframes=\(summary.keyframes) \
            sentBytes=\(summary.sentBytes) encodeSeconds=\(encodeSeconds)
            """
        log.info("\(stoppedMessage, privacy: .public)")
    }

    func resumeAfterClientRefresh() {
        overlayState?.set(fullScreen: false)
        encodeQueue.async { [weak self] in
            guard let self, !self.stopped else { return }
            self.lock.withLock { self.keyframeState.clientResumed() }
            self.resumeKeyframePending = true
            self.resumeKeyframeInFlight = false
            self.resumeKeyframeAttempts = 0
            self.resumeKeyframeAttemptID &+= 1
            self.encodeCachedResumeFrame()
        }
    }

    private func handle(_ buffer: CVPixelBuffer, _ time: CMTime) {
        let frame = SendablePixelBuffer(value: buffer)
        encodeQueue.async { [weak self] in
            self?.encodeCapturedFrame(frame.value, time)
        }
    }

    private func encodeCapturedFrame(_ buffer: CVPixelBuffer, _ time: CMTime) {
        guard !stopped, let encoder else { return }
        let time = cachedFrame.capture(buffer, at: time, frameRate: frameRate)
        let captured = lock.withLock { counters.capturedFrames += 1; return counters.capturedFrames }
        if captured == 1 {
            let message = "Video session \(self.sessionID) received first captured frame pts=\(time.seconds)"
            log.info("\(message, privacy: .public)")
        }
        if resumeKeyframePending {
            encodeCachedResumeFrame()
            return
        }
        guard server.canAcceptVideo else {
            lock.withLock { counters.skippedFrames += 1 }
            if captured <= 3 {
                let message = "Video session \(self.sessionID) skipped frame before encode: "
                    + "EGFX not ready or backpressure"
                log.debug("\(message, privacy: .public)")
            }
            return
        }
        let (forceKeyframe, text) = lock.withLock { () -> (Bool, String) in
            let force = keyframeState.needsKeyframe
            return (force, legacyOverlayText)
        }
        var frame = buffer
        let referenceGeneration = lock.withLock {
            frameGeneration.submitted(keyframe: forceKeyframe)
        }
        let presentation = overlayPresentation(fallbackText: text)
        if presentation.enabled,
           let drawn = overlay.render(presentation.text,
                                      layout: presentation.fullScreen ? .fullScreen : .topLeft,
                                      onto: buffer) {
            frame = drawn
        }
        encoder.encode(frame, presentationTime: time, forceKeyframe: forceKeyframe,
                       referenceGeneration: referenceGeneration)
    }

    private func encodeCachedResumeFrame() {
        guard !stopped, resumeKeyframePending, !resumeKeyframeInFlight,
              let encoder, let (buffer, time) = cachedFrame.replay(frameRate: frameRate) else { return }
        guard server.canAcceptVideo else {
            scheduleResumeKeyframeRetry()
            return
        }
        resumeKeyframeInFlight = true
        resumeKeyframeAttemptID &+= 1
        let attemptID = resumeKeyframeAttemptID
        var frame = buffer
        let referenceGeneration = lock.withLock {
            frameGeneration.submitted(keyframe: true)
        }
        let text = lock.withLock { legacyOverlayText }
        let presentation = overlayPresentation(fallbackText: text)
        if presentation.enabled,
           let drawn = overlay.render(presentation.text,
                                      layout: presentation.fullScreen ? .fullScreen : .topLeft,
                                      onto: buffer) {
            frame = drawn
        }
        log.info("Replaying cached frame as an IDR after client output resumed")
        encoder.encode(frame, presentationTime: time, forceKeyframe: true,
                       referenceGeneration: referenceGeneration)
        encodeQueue.asyncAfter(deadline: .now() + .milliseconds(500)) { [weak self] in
            guard let self, self.resumeKeyframePending, self.resumeKeyframeInFlight,
                  self.resumeKeyframeAttemptID == attemptID else { return }
            self.resumeKeyframeInFlight = false
            self.log.notice("Timed out waiting for the client-resume IDR")
            self.scheduleResumeKeyframeRetry()
        }
    }

    private func deliver(_ frame: EncodedFrame, generation: UInt64) {
        encodeQueue.async { [weak self] in
            guard let self, !self.stopped, self.encoderGeneration == generation else { return }
            self.deliverOnEncodeQueue(frame)
        }
    }

    private func deliverOnEncodeQueue(_ frame: EncodedFrame) {
        let generationCurrent = lock.withLock {
            frameGeneration.delivered(keyframe: frame.isKeyframe,
                                       generation: frame.referenceGeneration)
        }
        guard generationCurrent else {
            log.debug("Discarded stale encoded frame after a newer keyframe")
            return
        }
        let result = server.submitH264(frame.data, keyframe: frame.isKeyframe,
                                       width: contentWidth, height: contentHeight)
        let (encoded, statusChanged) = lock.withLock { () -> (Int, Bool) in
            counters.encodedFrames += 1
            counters.encodeSeconds += frame.encodeDuration
            let changed = lastSubmitResult != result
            lastSubmitResult = result
            return (counters.encodedFrames, changed)
        }
        if statusChanged || encoded <= 3 || encoded.isMultiple(of: 30) || frame.isKeyframe {
            let message = "Video session \(self.sessionID) frame=\(encoded) keyframe=\(frame.isKeyframe) "
                + "bytes=\(frame.data.count) submit=\(String(describing: result))"
            log.info("\(message, privacy: .public)")
        }
        if result == .sent, !lastReportedFirstFrame {
            lastReportedFirstFrame = true
            onFirstFrameSent?()
        }
        lock.withLock {
            if result == .sent {
                counters.sentFrames += 1
                successfulFramesSinceRecovery += 1
                counters.sentBytes += frame.data.count
            }
            if result == .sent, frame.isKeyframe { counters.keyframes += 1 }
            keyframeState.submission(frameIsKeyframe: frame.isKeyframe, result: result)
        }
        if result == .sent, successfulFramesSinceRecovery >= 120 {
            encoderRecoveryAttempts = 0
            successfulFramesSinceRecovery = 0
        }
        if frame.isKeyframe {
            encodeQueue.async { [weak self] in
                guard let self else { return }
                self.resumeKeyframeInFlight = false
                if result == .sent {
                    self.resumeKeyframePending = false
                } else if self.resumeKeyframePending {
                    self.scheduleResumeKeyframeRetry()
                }
            }
        }
    }

    private func encoderFailed(_ error: EncoderError, generation: UInt64) {
        encodeQueue.async { [weak self] in
            guard let self, !self.stopped, self.encoderGeneration == generation else { return }
            self.lock.withLock { self.keyframeState.encoderFailed() }
            let message = "Video session \(self.sessionID) encoder failure: \(error.localizedDescription)"
            self.log.error("\(message, privacy: .public)")
            guard error.requiresSessionRebuild else {
                if self.resumeKeyframePending {
                    self.resumeKeyframeAttemptID &+= 1
                    self.resumeKeyframeInFlight = false
                    self.scheduleResumeKeyframeRetry()
                }
                return
            }
            self.rebuildEncoder()
        }
    }

    private func rebuildEncoder() {
        guard !encoderRestartInFlight, let settings = encoderSettings else { return }
        encoderRestartInFlight = true
        log.notice("Video session \(self.sessionID) discarding failed VideoToolbox session and rebuilding it")

        let failedEncoder = encoder
        encoder = nil
        encoderGeneration &+= 1
        resumeKeyframePending = true
        resumeKeyframeInFlight = false
        resumeKeyframeAttempts = 0
        resumeKeyframeAttemptID &+= 1
        // The keyframe resets the reference chain, so pending output from this failed
        // session can be dropped. Invalidate immediately instead of waiting for a
        // malfunctioning encoder to drain its queue.
        failedEncoder?.invalidate()
        restartEncoder(settings: settings)
    }

    private func restartEncoder(settings: H264Encoder.Settings) {
        encoderRecoveryAttempts += 1
        var replacementSettings = settings
        if settings.requireHardware && encoderRecoveryAttempts > 3 {
            replacementSettings.requireHardware = false
            let message = "Video session \(self.sessionID) hardware encoder repeatedly failed; "
                + "allowing software H.264 fallback"
            log.notice("\(message, privacy: .public)")
        }
        let generation = encoderGeneration
        do {
            let replacement = try makeEncoder(replacementSettings, generation: generation)
            guard !stopped, encoderGeneration == generation else {
                replacement.flush()
                encoderRestartInFlight = false
                return
            }
            encoderSettings = replacementSettings
            encoder = replacement
            encoderRestartInFlight = false
            successfulFramesSinceRecovery = 0
            let hardware = replacement.isHardwareAccelerated.map(String.init) ?? "unknown"
            let message = "Video session \(self.sessionID) rebuilt encoder "
                + "id=\(replacement.encoderID) hardware=\(hardware); forcing IDR"
            log.notice("\(message, privacy: .public)")
            encodeCachedResumeFrame()
        } catch {
            encoderRestartInFlight = false
            guard !stopped, encoderRecoveryAttempts < 8 else {
                let message = "Video session \(self.sessionID) encoder recovery exhausted: "
                    + error.localizedDescription
                log.error("\(message, privacy: .public)")
                return
            }
            let attempt = encoderRecoveryAttempts
            let delay = min(2.0, 0.1 * pow(2.0, Double(attempt - 1)))
            let delayText = delay.formatted(.number.precision(.fractionLength(2)))
            let message = "Video session \(self.sessionID) encoder recovery retry \(attempt) in \(delayText)s"
            log.notice("\(message, privacy: .public)")
            encodeQueue.asyncAfter(deadline: .now() + delay) { [weak self] in
                guard let self, !self.stopped, self.encoder == nil,
                      self.encoderGeneration == generation else { return }
                self.encoderRestartInFlight = true
                self.restartEncoder(settings: settings)
            }
        }
    }

    private func scheduleResumeKeyframeRetry() {
        resumeKeyframeAttempts += 1
        guard resumeKeyframeAttempts < 40 else {
            resumeKeyframePending = false
            log.notice("Client refresh keyframe deferred until the next captured frame")
            return
        }
        encodeQueue.asyncAfter(deadline: .now() + .milliseconds(50)) { [weak self] in
            self?.encodeCachedResumeFrame()
        }
    }

    private func overlayPresentation(fallbackText: String) -> AtomicOverlayState.Snapshot {
        if let overlayState {
            return overlayState.get()
        }
        return AtomicOverlayState.Snapshot(
            enabled: overlayEnabled(), text: fallbackText, fullScreen: false)
    }
}
