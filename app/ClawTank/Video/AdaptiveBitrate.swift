// Adapts the encoder bit rate to the network, once per second, from signals the core measures:
// frames skipped for backpressure, frames sent but not yet acknowledged, and the smoothed
// send→ack time (EGFX frame acknowledgements, which include the client's decode time).
//
// Policy: back off quickly (×0.7) on congestion, recover slowly (×1.15 after 3 healthy
// seconds), never below a floor or above the preset's target. Latency is judged relative to the
// best latency seen this session, so a slow-but-steady link isn't mistaken for congestion.

import Foundation

struct AdaptiveBitrate: Equatable {
    struct Sample: Equatable {
        var skippedFrames: Int
        var framesInFlight: Int
        var ackLatencyMs: Double
        var framesSent: Int
        /// Smoothed EGFX payload goodput in bits per second; zero means no ACK sample.
        var acknowledgedGoodputBps: Double
    }

    let target: Int
    let floor: Int
    private(set) var current: Int
    /// Consecutive one-second samples with both submitted and acknowledged payload.
    /// Missing ACKs are unknown capacity, so they must not advance recovery.
    private var healthySeconds = 0
    private var bestLatencyMs = Double.infinity
    private(set) var acknowledgedGoodputBps = 0.0

    init(target: Int) {
        self.target = target
        self.floor = max(1_000_000, target / 6)
        self.current = target
    }

    /// True when the last sample showed congestion (for the stats readout).
    private(set) var congested = false

    /// Feeds one second of measurements; returns the new bit rate if it changed.
    mutating func update(_ sample: Sample) -> Int? {
        if sample.ackLatencyMs > 0 { bestLatencyMs = min(bestLatencyMs, sample.ackLatencyMs) }
        if sample.acknowledgedGoodputBps > 0 {
            let measured = sample.acknowledgedGoodputBps
            acknowledgedGoodputBps = acknowledgedGoodputBps == 0
                ? measured
                : acknowledgedGoodputBps * 0.75 + measured * 0.25
        }
        let latencyLimit = max(80, (bestLatencyMs.isFinite ? bestLatencyMs : 0) * 3)
        congested = sample.skippedFrames > 2 || sample.framesInFlight > 6
            || (sample.ackLatencyMs > 0 && sample.ackLatencyMs > latencyLimit)
        let previous = current
        if congested {
            healthySeconds = 0
            current = max(floor, Int(Double(current) * 0.7))
        } else if sample.framesSent > 0, sample.acknowledgedGoodputBps > 0 {
            healthySeconds += 1
            // A missing ACK is unknown capacity, not evidence of health. Probe only after three
            // consecutive acknowledged intervals; goodput is payload delivery, not total link
            // capacity and therefore does not independently trigger a backoff.
            if healthySeconds >= 3, current < target {
                current = min(target, Int(Double(current) * 1.15))
                healthySeconds = 0
            }
        } else if sample.framesSent > 0 {
            healthySeconds = 0
        } else {
            healthySeconds = 0
        }
        return current == previous ? nil : current
    }
}
