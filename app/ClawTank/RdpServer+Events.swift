// RdpServer: decoding core events (kind + detail string) into `RdpServer.Event`.

import ClawTankCore
import Foundation

extension RdpServer {
    static func decodeFiles(_ detail: String) -> [RemoteFile] {
        detail.split(separator: "\n").compactMap { line in
            let parts = line.split(separator: "\t", maxSplits: 2).map(String.init)
            guard parts.count == 3, let size = UInt64(parts[0]) else { return nil }
            return RemoteFile(path: parts[2], size: size, isDirectory: parts[1] == "1")
        }
    }

    static func decodeFileRequest(_ detail: String) -> Event? {
        let parts = detail.split(separator: "\t").map(String.init)
        guard parts.count >= 3, let stream = UInt32(parts[0]), let index = Int(parts[1]) else { return nil }
        if parts[2] == "size" { return .clipboardFileRequested(stream: stream, index: index, query: .size) }
        guard parts.count == 5, let position = UInt64(parts[3]), let length = Int(parts[4]) else { return nil }
        let query = FileQuery.range(position: position, length: length)
        return .clipboardFileRequested(stream: stream, index: index, query: query)
    }

    static func decodeMedia(kind: CtEventKind, detail: String) -> Event? {
        switch kind {
        case CT_EVENT_KIND_AUDIO_START:
            let parts = detail.split(separator: "x").compactMap { Int($0) }
            return parts.count == 2 ? .audioStart(rate: parts[0], channels: parts[1]) : .error("bad audio \(detail)")
        case CT_EVENT_KIND_AUDIO_STOP: return .audioStop
        default: return decodeClipboard(kind: kind, detail: detail)
        }
    }

    static func decodeClipboard(kind: CtEventKind, detail: String) -> Event? {
        switch kind {
        case CT_EVENT_KIND_CLIPBOARD_READY: return .clipboardReady
        case CT_EVENT_KIND_CLIPBOARD_REMOTE_FORMATS:
            let formats = detail.split(separator: "\n").compactMap { line -> ClipboardFormatRef? in
                let parts = line.split(separator: "\t", maxSplits: 1).map(String.init)
                guard let id = UInt32(parts[0]) else { return nil }
                return ClipboardFormatRef(id: id, name: parts.count > 1 ? parts[1] : nil)
            }
            return .clipboardRemoteFormats(formats)
        case CT_EVENT_KIND_CLIPBOARD_DATA_REQUESTED: return UInt32(detail).map { .clipboardDataRequested(format: $0) }
        case CT_EVENT_KIND_CLIPBOARD_DATA: return UInt32(detail).map { .clipboardData(format: $0) }
        case CT_EVENT_KIND_CLIPBOARD_REMOTE_FILES: return .clipboardRemoteFiles(decodeFiles(detail))
        case CT_EVENT_KIND_CLIPBOARD_FILE_REQUESTED: return decodeFileRequest(detail)
        case CT_EVENT_KIND_CLIPBOARD_FILE_CONTENTS: return UInt32(detail).map { .clipboardFileContents(stream: $0) }
        default: return nil
        }
    }

    static func decodeVideo(kind: CtEventKind, detail: String) -> Event? {
        if let media = decodeMedia(kind: kind, detail: detail) { return media }
        if kind == CT_EVENT_KIND_RDP_DIAGNOSTICS { return decodeDiagnostics(detail) }
        return decodeVideoEvent(kind: kind, detail: detail)
    }

    private static func decodeVideoEvent(kind: CtEventKind, detail: String) -> Event? {
        switch kind {
        case CT_EVENT_KIND_VIDEO_START, CT_EVENT_KIND_VIDEO_RESIZE:
            let parts = detail.split(whereSeparator: { $0 == "x" || $0 == "@" }).compactMap { Int($0) }
            guard parts.count == 3 else { return .error("bad video size \(detail)") }
            let scale = parts[2] > 0 ? parts[2] : nil
            return decodeVideoSize(kind: kind, width: parts[0], height: parts[1], scale: scale)
        case CT_EVENT_KIND_VIDEO_STOP: return .videoStop
        case CT_EVENT_KIND_VIDEO_REFRESH: return .videoRefresh
        case CT_EVENT_KIND_VIDEO_TRANSPORT:
            guard detail == "udp" || detail == "tcp" else {
                return .error("bad video transport \(detail)")
            }
            return .videoTransport(detail)
        case CT_EVENT_KIND_INITIAL_SCREEN:
            let parts = detail.split(separator: "x").compactMap { Int($0) }
            guard parts.count == 2 else { return .error("bad initial screen \(detail)") }
            return .initialScreen(width: parts[0], height: parts[1])
        default:
            return nil
        }
    }

    private static func decodeVideoSize(kind: CtEventKind, width: Int, height: Int,
                                        scale: Int?) -> Event {
        if kind == CT_EVENT_KIND_VIDEO_START {
            return .videoStart(width: width, height: height, scalePercent: scale)
        }
        return .videoResize(width: width, height: height, scalePercent: scale)
    }

    static func decodeBitmap(kind: CtEventKind, detail: String) -> Event? {
        let parts = detail.split(whereSeparator: { $0 == "x" || $0 == "@" }).compactMap { Int($0) }
        guard parts.count == 3 else {
            return decodeBadBitmapEvent(kind: kind, detail: detail)
        }
        let scale = parts[2] > 0 ? parts[2] : nil
        switch kind {
        case CT_EVENT_KIND_PROGRESSIVE_START:
            return .progressiveStart(width: parts[0], height: parts[1], scalePercent: scale)
        case CT_EVENT_KIND_PROGRESSIVE_RESIZE:
            return .progressiveResize(width: parts[0], height: parts[1], scalePercent: scale)
        case CT_EVENT_KIND_BITMAP_START:
            return .bitmapStart(width: parts[0], height: parts[1], scalePercent: scale)
        case CT_EVENT_KIND_BITMAP_RESIZE:
            return .bitmapResize(width: parts[0], height: parts[1], scalePercent: scale)
        default:
            return nil
        }
    }

    static func decodeBadBitmapEvent(kind: CtEventKind, detail: String) -> Event? {
        switch kind {
        case CT_EVENT_KIND_PROGRESSIVE_START:
            return .error("bad progressive size \(detail)")
        case CT_EVENT_KIND_PROGRESSIVE_RESIZE:
            return .error("bad progressive resize \(detail)")
        case CT_EVENT_KIND_BITMAP_START:
            return .error("bad bitmap size \(detail)")
        case CT_EVENT_KIND_BITMAP_RESIZE:
            return .error("bad bitmap resize \(detail)")
        default:
            return nil
        }
    }

    static func decodeDiagnostics(_ detail: String) -> Event? {
        let values = diagnosticsFields(detail)
        func int(_ key: String) -> Int? { values[key].flatMap(Int.init) }
        func uint32(_ key: String) -> UInt32? { values[key].flatMap(UInt32.init) }
        func uint64(_ key: String) -> UInt64? { values[key].flatMap(UInt64.init) }
        func bool(_ key: String) -> Bool? { int(key).map { $0 != 0 } }
        func text(_ key: String) -> String? {
            guard let value = values[key], value != "unknown", value != "none" else { return nil }
            return value
        }
        guard let width = int("width"), let height = int("height") else { return nil }
        var state = RdpDiagnostics()
        state.width = width
        state.height = height
        state.scalePercent = int("scale")
        state.deviceScalePercent = int("device_scale")
        state.clientInfoFlags = uint32("client_info_flags") ?? 0
        state.clientEarlyCapabilityFlags = uint32("client_early_capability_flags") ?? 0
        state.performanceFlags = uint32("performance_flags") ?? 0
        state.monitorCount = int("monitors") ?? 0
        decodeTransportDiagnostics(values, into: &state)
        decodeEgfxDiagnostics(values, into: &state)
        decodeNetworkDiagnostics(values, into: &state)
        state.transport = text("transport") ?? "tcp"
        return .rdpDiagnostics(state)
    }

    private static func decodeTransportDiagnostics(
        _ values: [String: String], into state: inout RdpDiagnostics
    ) {
        func int(_ key: String) -> Int? { values[key].flatMap(Int.init) }
        func uint32(_ key: String) -> UInt32? { values[key].flatMap(UInt32.init) }
        func bool(_ key: String) -> Bool? { int(key).map { $0 != 0 } }
        state.clientSupportsUDPFECR = bool("client_udp_fecr") ?? false
        state.clientSupportsSoftSync = bool("client_soft_sync") ?? false
        state.clientCapabilitiesKnown = bool("client_capabilities_known") ?? false
        state.multitransportOffered = bool("multitransport_offered") ?? false
        state.multitransportResponse = bool("multitransport_response")
    }

    private static func decodeEgfxDiagnostics(
        _ values: [String: String], into state: inout RdpDiagnostics
    ) {
        func uint32(_ key: String) -> UInt32? { values[key].flatMap(UInt32.init) }
        func bool(_ key: String) -> Bool? {
            values[key].flatMap(Int.init).map { $0 != 0 }
        }
        func text(_ key: String) -> String? {
            guard let value = values[key], value != "unknown", value != "none" else { return nil }
            return value
        }
        state.egfxOpen = bool("egfx_open") ?? false
        state.egfxChannelID = uint32("egfx_channel")
        state.egfxCapabilityVersions = values["egfx_versions"]?
            .split(separator: ",").compactMap { UInt32($0) } ?? []
        state.egfxCapabilityVersion = uint32("egfx_version")
        state.egfxAVC420 = bool("egfx_avc420") ?? false
        state.egfxAVC444 = bool("egfx_avc444") ?? false
        state.egfxProgressive = bool("egfx_progressive") ?? false
        state.egfxBitmap = bool("egfx_bitmap") ?? false
        state.legacyRemoteFX = bool("legacy_remotefx") ?? false
        state.legacyImageRemoteFX = bool("legacy_image_remotefx") ?? false
        state.legacyNSCodec = bool("legacy_nscodec") ?? false
        state.legacyQOI = bool("legacy_qoi") ?? false
        state.legacyQOIZ = bool("legacy_qoiz") ?? false
        state.legacySurfaceCommands = uint32("legacy_surface_commands") ?? 0
        state.legacySelectedCodec = text("legacy_selected")
        state.selectedVideoPath = text("selected")
    }

    private static func decodeNetworkDiagnostics(
        _ values: [String: String], into state: inout RdpDiagnostics
    ) {
        func int(_ key: String) -> Int? { values[key].flatMap(Int.init) }
        func uint32(_ key: String) -> UInt32? { values[key].flatMap(UInt32.init) }
        func uint64(_ key: String) -> UInt64? { values[key].flatMap(UInt64.init) }
        func bool(_ key: String) -> Bool? { int(key).map { $0 != 0 } }
        func text(_ key: String) -> String? {
            guard let value = values[key], value != "unknown", value != "none" else { return nil }
            return value
        }
        state.clientQueueDepth = uint32("client_queue_depth")
        state.clientAckSuspended = bool("client_ack_suspended") ?? false
        state.clientFramesDecoded = uint32("client_frames_decoded")
        state.clientDecodeRenderMilliseconds = uint32("client_decode_render_ms").flatMap {
            UInt16(exactly: $0)
        }
        state.clientQoEReports = uint64("client_qoe_reports") ?? 0
        state.networkRTTMilliseconds = uint32("network_rtt_ms")
        state.networkBaselineRTTMilliseconds = uint32("network_baseline_rtt_ms")
        state.networkBandwidthKilobitsPerSecond = uint32("network_bandwidth_kbps")
        state.clientRDPVersion = uint32("client_rdp_version")
        state.clientBuild = uint32("client_build")
        state.clientRequestedWidth = int("client_requested_width")
        state.clientRequestedHeight = int("client_requested_height")
        state.clientColorDepth = text("client_color_depth")
        state.clientConnectionType = text("client_connection_type")
        state.clientPhysicalWidthMillimeters = uint32("client_physical_width_mm")
        state.clientPhysicalHeightMillimeters = uint32("client_physical_height_mm")
        state.clientOrientationDegrees = uint32("client_orientation_degrees").flatMap {
            UInt16(exactly: $0)
        }
    }

    nonisolated static func diagnosticsFields(_ detail: String) -> [String: String] {
        var values: [String: String] = [:]
        for line in detail.split(separator: "\n") {
            let parts = line.split(separator: "=", maxSplits: 1).map(String.init)
            guard parts.count == 2 else { continue }
            values[parts[0]] = parts[1]
        }
        return values
    }

    static func decode(kind: CtEventKind, detail: String) -> Event {
        let parts = detail.split(separator: "\n", maxSplits: 2).map(String.init)
        if let bitmap = decodeBitmap(kind: kind, detail: detail) { return bitmap }
        if let video = decodeVideo(kind: kind, detail: detail) { return video }
        return decodeConnection(kind: kind, detail: detail, parts: parts)
    }

    private static func decodeConnection(kind: CtEventKind, detail: String, parts: [String]) -> Event {
        switch kind {
        case CT_EVENT_KIND_LISTENING: return .listening(detail)
        case CT_EVENT_KIND_REJECTED:
            let reason = parts.dropFirst().joined(separator: "\n")
            return .rejected(peer: parts.first ?? "", reason: reason)
        case CT_EVENT_KIND_LOCKED_OUT:
            return .lockedOut(ip: parts.first ?? "", minutes: Int(parts.last ?? "") ?? 0)
        case CT_EVENT_KIND_CLIENT_CONNECTED: return .clientConnected(detail)
        case CT_EVENT_KIND_CLIENT_DISCONNECTED:
            return .clientDisconnected(peer: parts.first ?? "", error: parts.count > 1 ? parts[1] : nil)
        case CT_EVENT_KIND_CONNECTION_FAILED:
            let hasClassification = parts.count > 2
            return .connectionFailed(
                peer: parts.first ?? "",
                classification: hasClassification ? parts[1] : "",
                error: (hasClassification ? parts[2] : parts.dropFirst().joined(separator: "\n"))
            )
        case CT_EVENT_KIND_STOPPED: return .stopped
        default: return .error(detail)
        }
    }
}
