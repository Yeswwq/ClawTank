// Streams bounded BGRA screen frames through RemoteFX Progressive or the legacy bitmap path.

import CoreVideo
import Foundation
import os

private struct SendableBitmapBuffer: @unchecked Sendable {
    let value: CVPixelBuffer
    let sequence: UInt64
    let capturedAtNanoseconds: UInt64
}

private enum RejectionReason: String {
    case stopped
    case coreNotReady
    case layoutMismatch
    case lockFailed
    case noBaseAddress
    case storageTooSmall
    case coreRejected
}

private struct BitmapRejectionCounts {
    var stopped = 0
    var coreNotReady = 0
    var layoutMismatch = 0
    var lockFailed = 0
    var noBaseAddress = 0
    var storageTooSmall = 0
    var coreRejected = 0
}

final class BitmapSession: @unchecked Sendable {
    static let frameRate = 10

    enum Transport {
        case progressive
        case legacyBitmap
    }

    let contentWidth: Int
    let contentHeight: Int
    let target: DisplayTarget
    let transport: Transport

    private let server: RdpServer
    private let lock = NSLock()
    private let submitQueue = DispatchQueue(label: "dev.clawtank.bitmap-submit", qos: .userInitiated)
    private var capture: ScreenCapture?
    private var timer: DispatchSourceTimer?
    private var latestBuffer: SendableBitmapBuffer?
    private var nextSequence: UInt64 = 0
    private var lastSubmittedSequence: UInt64 = 0
    private var refreshGeneration: UInt64 = 0
    private var servedRefreshGeneration: UInt64 = 0
    private var stopped = false
    private var submittedFrames = 0
    private var skippedFrames = 0
    private var overwrittenFrames = 0
    private var rejectionCounts = BitmapRejectionCounts()
    private let onFirstFrameSent: (@Sendable () -> Void)?
    private var reportedFirstFrame = false
    private let log = Logger(subsystem: "dev.clawtank.ClawTank", category: "bitmap")

    init(server: RdpServer, width: Int, height: Int, target: DisplayTarget,
         transport: Transport = .legacyBitmap, onFirstFrameSent: (@Sendable () -> Void)? = nil) {
        self.server = server
        self.contentWidth = width
        self.contentHeight = height
        self.target = target
        self.transport = transport
        self.onFirstFrameSent = onFirstFrameSent
    }

    func start() async throws {
        let capture = ScreenCapture(
            onFrame: { [weak self] buffer, _ in
                self?.handle(buffer)
            },
            onStop: { [weak self] error in
                if let error {
                    self?.log.error("Bitmap capture ended: \(error.localizedDescription, privacy: .public)")
                }
            })
        self.capture = capture
        do {
            try await capture.start(.init(
                contentWidth: contentWidth,
                contentHeight: contentHeight,
                codedWidth: contentWidth,
                codedHeight: contentHeight,
                frameRate: Self.frameRate,
                pixelFormat: kCVPixelFormatType_32BGRA,
                displaySize: target.bounds.size), displayID: target.displayID)
        } catch {
            if self.capture === capture { self.capture = nil }
            await capture.stop()
            throw error
        }
        startSubmissionTimer()
        let started = "\(transportName) capture started "
            + "size=\(contentWidth)x\(contentHeight) fps=\(Self.frameRate)"
        log.info("\(started, privacy: .public)")
    }

    func stop() async {
        let timer = lock.withLock { () -> DispatchSourceTimer? in
            let timer = self.timer
            self.timer = nil
            stopped = true
            latestBuffer = nil
            return timer
        }
        timer?.cancel()
        submitQueue.sync {}
        await capture?.stop()
        capture = nil
        let summary = lock.withLock { (submittedFrames, skippedFrames, overwrittenFrames) }
        let stoppedMessage = "\(transportName) capture stopped submitted=\(summary.0) "
            + "skipped=\(summary.1) overwritten=\(summary.2)"
        log.info("\(stoppedMessage, privacy: .public)")
        logRejectionSummaryIfNeeded()
    }

    func refresh() {
        lock.withLock {
            rejectionCounts = BitmapRejectionCounts()
            nextSequence &+= 1
        }
        let shouldRefresh = lock.withLock { () -> Bool in
            guard !stopped else { return false }
            refreshGeneration &+= 1
            return true
        }
        guard shouldRefresh else { return }
        submitQueue.async { [weak self] in self?.submitLatestFrame() }
    }

    private func handle(_ buffer: CVPixelBuffer) {
        lock.withLock {
            guard !stopped else { return }
            if let pending = latestBuffer, pending.sequence > lastSubmittedSequence {
                overwrittenFrames += 1
            }
            nextSequence &+= 1
            latestBuffer = SendableBitmapBuffer(value: buffer, sequence: nextSequence,
                                                capturedAtNanoseconds: DispatchTime.now().uptimeNanoseconds)
        }
    }

    private func startSubmissionTimer() {
        let timer = DispatchSource.makeTimerSource(queue: submitQueue)
        timer.schedule(deadline: .now() + .milliseconds(100), repeating: .milliseconds(100),
                       leeway: .milliseconds(10))
        timer.setEventHandler { [weak self] in self?.submitLatestFrame() }
        lock.withLock { self.timer = timer }
        timer.resume()
    }

    private func submitLatestFrame() {
        let snapshot = lock.withLock {
            () -> (SendableBitmapBuffer, UInt64)? in
            guard !stopped, let latestBuffer,
                  latestBuffer.sequence > lastSubmittedSequence
                      || refreshGeneration > servedRefreshGeneration else {
                return nil
            }
            return (latestBuffer, refreshGeneration)
        }
        guard let (frame, requestedRefreshGeneration) = snapshot else { return }
        let submitStarted = DispatchTime.now().uptimeNanoseconds
        guard submit(frame.value) else { return }
        let submitUs = (DispatchTime.now().uptimeNanoseconds - submitStarted) / 1_000
        let captureAgeUs = frame.capturedAtNanoseconds > 0
            ? (submitStarted - frame.capturedAtNanoseconds) / 1_000
            : 0
        lock.withLock {
            guard !stopped else { return }
            let shouldReportFirstFrame = !reportedFirstFrame
            if shouldReportFirstFrame { reportedFirstFrame = true }
            lastSubmittedSequence = max(lastSubmittedSequence, frame.sequence)
            servedRefreshGeneration = max(servedRefreshGeneration, requestedRefreshGeneration)
            submittedFrames += 1
            if submittedFrames == 1 || submittedFrames.isMultiple(of: 30) {
                let bytes = CVPixelBufferGetDataSize(frame.value)
                let stride = CVPixelBufferGetBytesPerRow(frame.value)
                let message = "\(self.transportName) frame submitted count=\(self.submittedFrames) "
                    + "bytes=\(bytes) stride=\(stride) capture_age_us=\(captureAgeUs) "
                    + "submit_us=\(submitUs) overwritten=\(self.overwrittenFrames)"
                log.info("\(message)")
            }
            if shouldReportFirstFrame { onFirstFrameSent?() }
        }
    }

    @discardableResult
    private func submit(_ pixelBuffer: CVPixelBuffer) -> Bool {
        guard !lock.withLock({ stopped }) else {
            lock.withLock { skippedFrames += 1 }
            recordRejection(.stopped)
            return false
        }
        let ready = transport == .progressive ? server.canAcceptProgressive : server.canAcceptBitmap
        guard ready else {
            lock.withLock { skippedFrames += 1 }
            recordRejection(.coreNotReady)
            return false
        }
        guard CVPixelBufferGetPixelFormatType(pixelBuffer) == kCVPixelFormatType_32BGRA,
              CVPixelBufferGetWidth(pixelBuffer) == contentWidth,
              CVPixelBufferGetHeight(pixelBuffer) == contentHeight else {
            recordRejection(.layoutMismatch)
            log.error("Bitmap frame layout changed unexpectedly")
            return false
        }

        let status = CVPixelBufferLockBaseAddress(pixelBuffer, [])
        guard status == kCVReturnSuccess else {
            recordRejection(.lockFailed)
            log.error("Could not lock bitmap frame: \(status)")
            return false
        }
        defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, []) }

        guard let baseAddress = CVPixelBufferGetBaseAddress(pixelBuffer) else {
            recordRejection(.noBaseAddress)
            log.error("Bitmap frame has no base address")
            return false
        }
        let stride = CVPixelBufferGetBytesPerRow(pixelBuffer)
        let length = CVPixelBufferGetDataSize(pixelBuffer)
        guard stride >= contentWidth * 4, length >= stride * contentHeight else {
            recordRejection(.storageTooSmall)
            log.error("Bitmap frame storage is smaller than its dimensions")
            return false
        }
        guard submitPreparedFrame(baseAddress, length: length, stride: stride) else {
            lock.withLock { skippedFrames += 1 }
            recordRejection(.coreRejected)
            return false
        }
        return true
    }

    private func submitPreparedFrame(_ baseAddress: UnsafeRawPointer, length: Int,
                                     stride: Int) -> Bool {
        switch transport {
        case .progressive:
            let result = server.submitProgressive(baseAddress, length: length, width: contentWidth,
                                                  height: contentHeight, stride: stride)
            return result == .sent
        case .legacyBitmap:
            return server.submitBitmap(baseAddress, length: length, width: contentWidth,
                                       height: contentHeight, stride: stride)
        }
    }

    private var transportName: String {
        transport == .progressive ? "RemoteFX Progressive" : "Legacy bitmap"
    }

    private func recordRejection(_ reason: RejectionReason) {
        lock.withLock {
            switch reason {
            case .stopped: rejectionCounts.stopped += 1
            case .coreNotReady: rejectionCounts.coreNotReady += 1
            case .layoutMismatch: rejectionCounts.layoutMismatch += 1
            case .lockFailed: rejectionCounts.lockFailed += 1
            case .noBaseAddress: rejectionCounts.noBaseAddress += 1
            case .storageTooSmall: rejectionCounts.storageTooSmall += 1
            case .coreRejected: rejectionCounts.coreRejected += 1
            }
        }
    }

    private func logRejectionSummaryIfNeeded() {
        let counts = lock.withLock { rejectionCounts }
        let total = counts.stopped + counts.coreNotReady + counts.layoutMismatch
            + counts.lockFailed + counts.noBaseAddress + counts.storageTooSmall + counts.coreRejected
        guard total > 0 else { return }
        let message = "\(transportName) frame rejections stopped=\(counts.stopped) "
            + "coreNotReady=\(counts.coreNotReady) layoutMismatch=\(counts.layoutMismatch) "
            + "lockFailed=\(counts.lockFailed) noBaseAddress=\(counts.noBaseAddress) "
            + "storageTooSmall=\(counts.storageTooSmall) coreRejected=\(counts.coreRejected)"
        log.info("\(message, privacy: .public)")
    }
}
