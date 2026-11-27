// Handles Tailscale discovery, remote-address actions and video statistics side paths.

import Foundation
import ClawTankCore

@MainActor
extension ServerController {
    struct VideoStatsDelta: Equatable, Sendable {
        let encodedFrames: Int
        let sentFrames: Int
        let sentBytes: Int
        let encodeSeconds: Double
        let skippedFrames: Int
    }

    /// Re-detects Tailscale and the wake setting (runs the CLI off the main thread).
    func refreshRemoteAccess() {
        Task.detached(priority: .utility) {
            let state = RemoteAccess.detect()
            let wake = RemoteAccess.wakeForNetworkAccess()
            await MainActor.run { [weak self] in
                self?.updateRemoteAccess(state, wakeForNetwork: wake)
            }
        }
    }

    func copyRemoteAddress() {
        if let address = remote.remoteAddress { Self.copyToPasteboard(address) }
    }

    func videoStatsDelta(_ now: VideoSession.Counters) -> VideoStatsDelta {
        let delta = VideoStatsDelta(
            encodedFrames: now.encodedFrames - lastCounters.encodedFrames,
            sentFrames: now.sentFrames - lastCounters.sentFrames,
            sentBytes: now.sentBytes - lastCounters.sentBytes,
            encodeSeconds: now.encodeSeconds - lastCounters.encodeSeconds,
            skippedFrames: now.skippedFrames - lastCounters.skippedFrames)
        lastCounters = now
        return delta
    }

    func updateVideoStatsFromCore(_ core: CtVideoStats, video: VideoSession,
                                  delta: VideoStatsDelta) -> String? {
        let ackedBytes = core.bytes_acked >= lastAckedBytes
            ? core.bytes_acked - lastAckedBytes
            : core.bytes_acked
        lastAckedBytes = core.bytes_acked
        let ackedFrames = core.frames_acked >= lastAckedFrames
            ? core.frames_acked - lastAckedFrames
            : core.frames_acked
        lastAckedFrames = core.frames_acked
        let uptime = ProcessInfo.processInfo.systemUptime
        let interval = max(0.1, uptime - lastStatsSampleUptime)
        lastStatsSampleUptime = uptime
        let sample = AdaptiveBitrate.Sample(
            skippedFrames: delta.skippedFrames,
            framesInFlight: Int(core.frames_in_flight),
            ackLatencyMs: ackedFrames > 0 ? Double(core.ack_latency_us) / 1000 : 0,
            framesSent: delta.sentFrames,
            acknowledgedGoodputBps: Double(ackedBytes) * 8 / interval)
        guard let state = video.adapt(sample) else { return nil }
        return Self.formatAdaptiveVideoStats(
            ackDelayMs: sample.acknowledgedGoodputBps > 0 ? sample.ackLatencyMs : 0,
            targetBitrate: state.bitRate,
            acknowledgedGoodputBps: sample.acknowledgedGoodputBps > 0
                ? state.acknowledgedGoodputBps : 0,
            congested: state.congested)
    }
}
