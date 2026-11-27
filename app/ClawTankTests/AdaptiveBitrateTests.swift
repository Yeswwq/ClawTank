import XCTest
@testable import ClawTank

final class AdaptiveBitrateTests: XCTestCase {
    private func sample(skipped: Int = 0, inFlight: Int = 1, latency: Double = 10, sent: Int = 60,
                        goodput: Double = 20_000_000)
        -> AdaptiveBitrate.Sample {
        .init(skippedFrames: skipped, framesInFlight: inFlight, ackLatencyMs: latency, framesSent: sent,
              acknowledgedGoodputBps: goodput)
    }

    func testBacksOffOnCongestionAndRecoversSlowly() {
        var rate = AdaptiveBitrate(target: 20_000_000)
        XCTAssertNil(rate.update(sample()), "healthy at target: no change")
        XCTAssertEqual(rate.update(sample(skipped: 10)), 14_000_000, "×0.7 on skipped frames")
        XCTAssertEqual(rate.update(sample(inFlight: 12)), 9_800_000, "×0.7 on a deep ack backlog")
        XCTAssertNil(rate.update(sample()))
        XCTAssertNil(rate.update(sample()))
        XCTAssertEqual(rate.update(sample()), 11_270_000, "×1.15 after 3 healthy seconds")
    }

    func testLatencyIsJudgedAgainstTheBestSeen() {
        var rate = AdaptiveBitrate(target: 10_000_000)
        _ = rate.update(sample(latency: 40))          // steady 40 ms link: fine
        XCTAssertNil(rate.update(sample(latency: 60)), "under 3× best and under 80 ms floor")
        XCTAssertNotNil(rate.update(sample(latency: 200)), "5× best: congested")
        XCTAssertTrue(rate.congested)
    }

    func testFloorAndCeiling() {
        var rate = AdaptiveBitrate(target: 12_000_000)
        for _ in 0..<20 { _ = rate.update(sample(skipped: 10)) }
        XCTAssertEqual(rate.current, 2_000_000, "never below target/6")
        for _ in 0..<60 { _ = rate.update(sample()) }
        XCTAssertEqual(rate.current, 12_000_000, "recovers to, not past, the target")
        XCTAssertNil(rate.update(sample(sent: 0, goodput: 0)), "idle seconds don't count as healthy or congested")
    }

    func testMissingAckDoesNotRecoverOrTreatStaticLowGoodputAsCongestion() {
        var rate = AdaptiveBitrate(target: 12_000_000)
        _ = rate.update(sample(skipped: 10))
        let backedOff = rate.current
        for _ in 0..<10 {
            XCTAssertNil(rate.update(sample(goodput: 0)), "no ACK must not probe upward")
        }
        XCTAssertEqual(rate.current, backedOff)

        for _ in 0..<2 {
            XCTAssertNil(rate.update(sample(goodput: 1_000_000)), "two ACK samples are not enough")
        }
        XCTAssertEqual(rate.current, backedOff)
        XCTAssertNotNil(rate.update(sample(goodput: 1_000_000)), "three acknowledged samples permit a probe")
        XCTAssertFalse(rate.congested, "low payload goodput alone is not congestion")
    }

    func testStaleAckLatencyDoesNotRepeatBackoffWithoutNewAcknowledgments() {
        var rate = AdaptiveBitrate(target: 10_000_000)
        _ = rate.update(sample(latency: 10))
        XCTAssertEqual(rate.update(sample(latency: 100)), 7_000_000)
        let backedOff = rate.current

        for _ in 0..<5 {
            XCTAssertNil(rate.update(sample(latency: 0, goodput: 0)),
                          "a missing ACK interval has no fresh latency sample")
            XCTAssertEqual(rate.current, backedOff)
        }
        XCTAssertFalse(rate.congested)
    }

    func testCongestionBackoffDoesNotTreatPayloadGoodputAsAvailableCapacity() {
        var rate = AdaptiveBitrate(target: 20_000_000)
        _ = rate.update(sample(goodput: 2_000_000))
        XCTAssertEqual(rate.update(sample(skipped: 10, goodput: 2_000_000)), 14_000_000)
    }
}
