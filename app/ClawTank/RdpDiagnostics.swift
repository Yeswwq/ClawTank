// Structured connection and codec facts reported by the Rust RDP core.

import Foundation

extension RdpServer {
    struct RdpDiagnostics: Equatable, Sendable {
        var width = 0
        var height = 0
        var scalePercent: Int?
        var deviceScalePercent: Int?
        var clientInfoFlags: UInt32 = 0
        var clientEarlyCapabilityFlags: UInt32 = 0
        var performanceFlags: UInt32 = 0
        var monitorCount = 0
        var clientSupportsUDPFECR = false
        var clientSupportsSoftSync = false
        var clientCapabilitiesKnown = false
        var multitransportOffered = false
        var multitransportResponse: Bool?
        var egfxOpen = false
        var egfxChannelID: UInt32?
        var egfxCapabilityVersions: [UInt32] = []
        var egfxCapabilityVersion: UInt32?
        var egfxAVC420 = false
        var egfxAVC444 = false
        var egfxProgressive = false
        var egfxBitmap = false
        var legacyRemoteFX = false
        var legacyImageRemoteFX = false
        var legacyNSCodec = false
        var legacyQOI = false
        var legacyQOIZ = false
        var legacySurfaceCommands: UInt32 = 0
        var legacySelectedCodec: String?
        var selectedVideoPath: String?
        var clientQueueDepth: UInt32?
        var clientAckSuspended = false
        var clientFramesDecoded: UInt32?
        var clientDecodeRenderMilliseconds: UInt16?
        var clientQoEReports: UInt64 = 0
        var networkRTTMilliseconds: UInt32?
        var networkBaselineRTTMilliseconds: UInt32?
        var networkBandwidthKilobitsPerSecond: UInt32?
        var clientRDPVersion: UInt32?
        var clientBuild: UInt32?
        var clientColorDepth: String?
        var clientConnectionType: String?
        var clientRequestedWidth: Int?
        var clientRequestedHeight: Int?
        var clientPhysicalWidthMillimeters: UInt32?
        var clientPhysicalHeightMillimeters: UInt32?
        var clientOrientationDegrees: UInt16?
        var transport = "tcp"
    }
}
