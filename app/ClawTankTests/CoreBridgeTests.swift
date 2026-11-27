import XCTest
import ClawTankCore
import Foundation
@testable import ClawTank

final class CoreBridgeTests: XCTestCase {
    func testVersionIsSemver() {
        let parts = CoreBridge.shared.version.split(separator: ".")
        XCTAssertEqual(parts.count, 3, "unexpected version: \(CoreBridge.shared.version)")
        XCTAssertTrue(parts.allSatisfy { Int($0) != nil })
    }

    func testInitIsIdempotent() {
        XCTAssertEqual(CoreBridge.shared.reinitialize(), CT_STATUS_ALREADY_INITIALIZED)
    }

    func testVideoEventsDecodeScaleHintAndUnknownScale() {
        XCTAssertEqual(
            RdpServer.decodeVideo(kind: CT_EVENT_KIND_VIDEO_START, detail: "1512x982@200"),
            .videoStart(width: 1512, height: 982, scalePercent: 200)
        )
        XCTAssertEqual(
            RdpServer.decodeVideo(kind: CT_EVENT_KIND_VIDEO_START, detail: "1512x982@0"),
            .videoStart(width: 1512, height: 982, scalePercent: nil)
        )
        XCTAssertEqual(
            RdpServer.decodeVideo(kind: CT_EVENT_KIND_VIDEO_RESIZE, detail: "1512x982@0"),
            .videoResize(width: 1512, height: 982, scalePercent: nil)
        )
    }

    func testProgressiveEventsAreDistinctFromLegacyBitmap() {
        XCTAssertEqual(
            RdpServer.decode(kind: CT_EVENT_KIND_PROGRESSIVE_START, detail: "1512x982@200"),
            .progressiveStart(width: 1512, height: 982, scalePercent: 200)
        )
        XCTAssertEqual(
            RdpServer.decode(kind: CT_EVENT_KIND_PROGRESSIVE_RESIZE, detail: "2000x1200@0"),
            .progressiveResize(width: 2000, height: 1200, scalePercent: nil)
        )
    }

    func testRdpDataTransportEventsDecode() {
        XCTAssertEqual(
            RdpServer.decodeVideo(kind: CT_EVENT_KIND_VIDEO_TRANSPORT, detail: "tcp"),
            .videoTransport("tcp")
        )
        XCTAssertEqual(
            RdpServer.decodeVideo(kind: CT_EVENT_KIND_VIDEO_TRANSPORT, detail: "udp"),
            .videoTransport("udp")
        )
        XCTAssertEqual(
            RdpServer.decodeVideo(kind: CT_EVENT_KIND_VIDEO_TRANSPORT, detail: "quic"),
            .error("bad video transport quic")
        )
    }

    func testRdpDiagnosticsFieldsDecode() {
        let detail = diagnosticFixture
        guard case .rdpDiagnostics(let state) = RdpServer.decodeVideo(
            kind: CT_EVENT_KIND_RDP_DIAGNOSTICS, detail: detail) else {
            return XCTFail("diagnostics event did not decode")
        }
        assertDiagnostics(state)
    }

    private var diagnosticFixture: String {
        """
            width=2316
            height=1080
            scale=200
            device_scale=180
            client_info_flags=213435
            client_early_capability_flags=400
            performance_flags=6
            monitors=1
            client_capabilities_known=1
            client_udp_fecr=1
            client_soft_sync=1
            multitransport_offered=1
            multitransport_response=1
            egfx_open=1
            egfx_channel=7
            egfx_versions=134285568,167936000
            egfx_version=167936000
            egfx_avc420=0
            egfx_progressive=1
            egfx_bitmap=0
            legacy_remotefx=0
            legacy_nscodec=1
            legacy_surface_commands=18
            legacy_selected=NSCodec
            selected=RemoteFXProgressive
            client_queue_depth=3
            client_frames_decoded=42
            client_ack_suspended=0
            client_decode_render_ms=17
            client_qoe_reports=2
            network_rtt_ms=18
            network_baseline_rtt_ms=14
            network_bandwidth_kbps=125000
            client_rdp_version=524292
            client_build=26000
            client_requested_width=2316
            client_requested_height=1080
            client_color_depth=32 bpp
            client_connection_type=LAN
            client_physical_width_mm=280
            client_physical_height_mm=210
            client_orientation_degrees=90
            transport=udp
            """
    }

    private func assertDiagnostics(_ state: RdpServer.RdpDiagnostics) {
        XCTAssertEqual(state.width, 2316)
        XCTAssertEqual(state.height, 1080)
        XCTAssertEqual(state.scalePercent, 200)
        XCTAssertEqual(state.deviceScalePercent, 180)
        XCTAssertEqual(state.monitorCount, 1)
        XCTAssertTrue(state.clientCapabilitiesKnown)
        XCTAssertTrue(state.clientSupportsUDPFECR)
        XCTAssertTrue(state.clientSupportsSoftSync)
        XCTAssertEqual(state.multitransportResponse, true)
        XCTAssertEqual(state.egfxChannelID, 7)
        XCTAssertTrue(state.egfxProgressive)
        XCTAssertFalse(state.egfxAVC420)
        XCTAssertEqual(state.legacySelectedCodec, "NSCodec")
        XCTAssertEqual(state.selectedVideoPath, "RemoteFXProgressive")
        XCTAssertEqual(state.clientEarlyCapabilityFlags, 400)
        XCTAssertEqual(state.clientQueueDepth, 3)
        XCTAssertEqual(state.clientFramesDecoded, 42)
        XCTAssertEqual(state.clientDecodeRenderMilliseconds, 17)
        XCTAssertEqual(state.clientQoEReports, 2)
        XCTAssertEqual(state.networkRTTMilliseconds, 18)
        XCTAssertEqual(state.networkBaselineRTTMilliseconds, 14)
        XCTAssertEqual(state.networkBandwidthKilobitsPerSecond, 125_000)
        XCTAssertEqual(state.clientRDPVersion, 524_292)
        XCTAssertEqual(state.clientBuild, 26_000)
        XCTAssertEqual(state.clientRequestedWidth, 2_316)
        XCTAssertEqual(state.clientRequestedHeight, 1_080)
        XCTAssertEqual(state.clientColorDepth, "32 bpp")
        XCTAssertEqual(state.clientConnectionType, "LAN")
        XCTAssertEqual(state.clientPhysicalWidthMillimeters, 280)
        XCTAssertEqual(state.clientPhysicalHeightMillimeters, 210)
        XCTAssertEqual(state.clientOrientationDegrees, 90)
        XCTAssertEqual(state.transport, "udp")
    }

    func testConnectionFailureClassificationSurvivesMultilineError() {
        XCTAssertEqual(
            RdpServer.decode(
                kind: CT_EVENT_KIND_CONNECTION_FAILED,
                detail: "10.0.0.2\nauthentication\n[cause @ server.rs:1] LogonDenied, caused by: bad password"
            ),
            .connectionFailed(
                peer: "10.0.0.2",
                classification: "authentication",
                error: "[cause @ server.rs:1] LogonDenied, caused by: bad password"
            )
        )
        XCTAssertEqual(
            RdpServer.decode(
                kind: CT_EVENT_KIND_CONNECTION_FAILED,
                detail: "10.0.0.2\n[old core error]"
            ),
            .connectionFailed(
                peer: "10.0.0.2",
                classification: "",
                error: "[old core error]"
            )
        )
    }
}
