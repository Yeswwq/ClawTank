// Renders the client-visible connection diagnostics overlay.

import CoreVideo
import Foundation
import os

extension ServerController {
    func stopCurrentVideo() async {
        statsTimer?.invalidate()
        statsTimer = nil
        videoStatus = nil
        guard let video else { return }
        self.video = nil
        await video.stop()
    }

    func publishStartedSession(_ request: VideoStartRequest, target: DisplayTarget) {
        let width = request.width
        let height = request.height
        displayDescription = target.isVirtual
            ? String(localized: "Virtual display \(width)×\(height)")
            : String(localized: "Physical display \(width)×\(height)")
        session?.resolution = "\(width)×\(height)"
        let transportName = switch request.transport {
        case .avc: "AVC420"
        case .progressive: "RemoteFXProgressive"
        case .bitmap: "legacy-bitmap"
        }
        let displayMessage = "Display \(width)x\(height) transport=\(transportName) " +
            "on \(target.displayID) virtual=\(target.isVirtual)"
        log.info("\(displayMessage, privacy: .public)")
    }

    func updateVideoStats() {
        guard let video else { return }
        let delta = videoStatsDelta(video.snapshot())
        var adaptiveStats: String?
        if let core = server?.videoStats {
            adaptiveStats = updateVideoStatsFromCore(core, video: video, delta: delta)
        }
        let text = Self.formatVideoStats(
            fps: delta.encodedFrames,
            bytesPerSecond: delta.sentBytes,
            encodeMs: delta.encodedFrames > 0
                ? delta.encodeSeconds * 1000 / Double(delta.encodedFrames) : 0,
            skipped: delta.skippedFrames,
            adaptiveStats: adaptiveStats,
            protocolStatus: Self.formatProtocolStatus(rdpDiagnostics))
        videoStatus = text
        updateOverlayDiagnostics()
    }

    func updateOverlayDiagnostics() {
        var diagnostics = Self.formatDiagnosticsOverlay(
            rdpDiagnostics, connected: session != nil, initial: !hasLiveVideoFrame,
            width: rdpDiagnostics.width, height: rdpDiagnostics.height)
        if let local = Self.localDiagnosticStatus(), !hasLiveVideoFrame {
            diagnostics += "\n\(String(localized: "Local capture/input")): \(local)"
        }
        overlayState.set(text: diagnostics)
        if let video {
            video.setOverlayText(diagnostics)
        }
    }

    func markLiveVideo() {
        guard !hasLiveVideoFrame else { return }
        hasLiveVideoFrame = true
        overlayState.set(fullScreen: false)
        updateOverlayDiagnostics()
        if let video {
            video.resumeAfterClientRefresh()
        }
    }

    func renderInitialDiagnostics(width: Int, height: Int) {
        guard let server else { return }
        var text = Self.formatDiagnosticsOverlay(
            rdpDiagnostics, connected: true, initial: true, width: width, height: height)
        if let local = Self.localDiagnosticStatus() {
            text += "\n\(String(localized: "Local capture/input")): \(local)"
        }
        overlayState.set(fullScreen: true)
        overlayState.set(text: text)
        guard let buffer = StatsOverlay().diagnosticBuffer(
            text, width: width, height: height) else {
            log.error("Could not render the initial diagnostic frame")
            return
        }
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        guard let base = CVPixelBufferGetBaseAddress(buffer) else { return }
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        let submitted = server.submitInitialBitmap(
            Data(bytes: base, count: CVPixelBufferGetDataSize(buffer)),
            width: width, height: height, stride: stride)
        let message = "Initial diagnostic frame rendered=\(submitted) size=\(width)x\(height)"
        log.info("\(message, privacy: .public)")
    }

    nonisolated static func localDiagnosticStatus() -> String? {
        let capturePermission = CGPreflightScreenCaptureAccess()
        let inputPermission = AXIsProcessTrusted()
        let capture = capturePermission
            ? String(localized: "screen capture granted")
            : String(localized: "screen capture denied")
        let input = inputPermission
            ? String(localized: "input granted")
            : String(localized: "input denied")
        return "\(capture) · \(input)"
    }
}
