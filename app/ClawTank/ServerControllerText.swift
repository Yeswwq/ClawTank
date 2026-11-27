// Pure helpers for ServerController: human wording for core events and errors, peer/host
// formatting. Static and side-effect free, so they're unit-tested directly.

import AppKit
import Foundation
import SystemConfiguration

extension ServerController {
    func copyPassword() { Self.copyToPasteboard(password) }
    func copyFingerprint() { Self.copyToPasteboard(fingerprint) }
    func copyAddress() { Self.copyToPasteboard(address) }

    static func copyToPasteboard(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }

    nonisolated static func describeRefusal(_ reason: String, ip: String) -> String {
        switch reason {
        case "lan-only": String(localized: "Blocked \(ip): not on your local network")
        case "allowlist": String(localized: "Blocked \(ip): not on the allowlist")
        case "locked-out": String(localized: "Blocked \(ip): locked out after failed logins")
        default: String(localized: "Blocked \(ip)")
        }
    }

    nonisolated static func connectionFailureOutcome(_ classification: String, _ raw: String) -> AuditEntry.Outcome {
        if classification == "authentication" {
            return .authenticationFailed
        }
        if classification == "handshake" {
            return .handshakeFailed
        }

        // Compatibility with events emitted by an older core without the
        // classification line. New cores always provide the explicit value.
        let text = raw.lowercased()
        if text.contains("broken pipe") || text.contains("close_notify")
            || text.contains("connection reset") || text.contains("unexpected eof") {
            return .handshakeFailed
        }
        if text.contains("logondenied") || text.contains("no candidate credential")
            || text.contains("credssp") || text.contains("credential validation failed")
            || text.contains("password credentials") {
            return .authenticationFailed
        }
        return .handshakeFailed
    }

    nonisolated static func describeFailure(_ raw: String, ip: String) -> String {
        let friendly = friendlyDetail(raw)
        if friendly == "Wrong username or password" {
            return String(localized: "Wrong username or password from \(ip)")
        }
        return String(localized: "Connection from \(ip) failed")
    }

    /// Short, human wording for common core errors (the raw text stays in the unified log).
    nonisolated static func friendlyDetail(_ raw: String) -> String {
        let text = raw.lowercased()
        // A CredSSP wrapper does not itself mean a bad password: a client
        // disconnect during the exchange appears as a Connector error too.
        if text.contains("close_notify") || text.contains("connection reset") || text.contains("broken pipe")
            || text.contains("unexpected eof") {
            return "Client closed the connection"
        }
        if text.contains("logondenied") || text.contains("no candidate credential") || text.contains("credssp") {
            return "Wrong username or password"
        }
        if text.contains("tls") || text.contains("certificate") {
            return "TLS handshake failed (client may have rejected the certificate)"
        }
        return raw.count > 120 ? String(raw.prefix(117)) + "…" : raw
    }

    /// "192.168.1.5:51234" → "192.168.1.5"; "[fe80::1]:5" → "fe80::1".
    nonisolated static func ipOnly(_ peer: String) -> String {
        if peer.hasPrefix("["), let end = peer.firstIndex(of: "]") {
            return String(peer[peer.index(after: peer.startIndex)..<end])
        }
        guard peer.filter({ $0 == ":" }).count == 1, let colon = peer.lastIndex(of: ":") else { return peer }
        return String(peer[..<colon])
    }

    /// The Bonjour name clients can use, e.g. "macmini.local".
    nonisolated static func localHostName() -> String {
        if let name = SCDynamicStoreCopyLocalHostName(nil) as String? {
            return "\(name).local"
        }
        return ProcessInfo.processInfo.hostName
    }

    nonisolated static func formatVideoStats(fps: Int, bytesPerSecond: Int, encodeMs: Double, skipped: Int,
                                             adaptiveStats: String? = nil,
                                             protocolStatus: String? = nil) -> String {
        let mbit = Double(bytesPerSecond * 8) / 1_000_000
        var lines = [String(localized: """
            \(fps) fps · \
            \(mbit, format: .number.precision(.fractionLength(1))) Mbit/s · \
            encoding \(encodeMs, format: .number.precision(.fractionLength(1))) ms
            """)]
        if skipped > 0 {
            lines.append(String(localized: "Skipped: \(skipped)"))
        }
        if let protocolStatus { lines.append(protocolStatus) }
        if let adaptiveStats { lines.append(adaptiveStats) }
        return lines.joined(separator: " · ")
    }

    nonisolated static func formatDiagnosticsOverlay(_ state: RdpServer.RdpDiagnostics,
                                                     connected: Bool) -> String {
        formatDiagnosticsOverlay(state, connected: connected, initial: false,
                                 width: state.width, height: state.height)
    }

    nonisolated static func formatProtocolStatus(_ state: RdpServer.RdpDiagnostics) -> String {
        let transport = state.transport == "udp" ? "UDP" : "TCP"
        let codec = state.selectedVideoPath ?? state.legacySelectedCodec
            ?? String(localized: "pending")
        let channel = state.egfxChannelID.map {
            String(format: String(localized: "EGFX channel %lld"), locale: .current, $0)
        } ?? String(localized: "EGFX pending")
        let capability = state.egfxCapabilityVersion.map { String(format: "EGFX 0x%08X", $0) }
            ?? String(localized: "EGFX pending")
        let udp = !state.clientCapabilitiesKnown
            ? String(localized: "UDP pending")
            : (state.clientSupportsUDPFECR
                ? String(localized: "UDP supported")
                : String(localized: "UDP unsupported"))
        return "\(channel) · \(capability) · \(codec) · \(transport) · \(udp)"
    }

    nonisolated static func formatDiagnosticsOverlay(_ state: RdpServer.RdpDiagnostics,
                                                     connected: Bool,
                                                     initial: Bool,
                                                     width: Int,
                                                     height: Int) -> String {
        let fields = diagnosticsFields(state, connected: connected, initial: initial,
                                       width: width, height: height)
        return """
            \(fields.heading)
            \(String(localized: "Connection")): \(fields.connection)
            \(String(localized: "Session")): \(fields.video) · \(fields.session)
            \(String(localized: "EGFX")): \(fields.egfx)
            \(String(localized: "Legacy bitmap path")): \(fields.legacy)
            \(String(localized: "Transport")): \(fields.transport)
            \(String(localized: "Client self-reported")): \(clientIdentity(state))
            \(String(localized: "Client formats")): \(fields.formats)
            \(String(localized: "Client feedback")): \(clientFeedback(state))
            \(String(localized: "Network path")): \(networkPath(state))
            \(String(localized: "Capability flags")): \(capabilityFlags(state))
            """
    }

    nonisolated private static func clientFeedback(_ state: RdpServer.RdpDiagnostics) -> String {
        let queue: String
        if state.clientAckSuspended {
            queue = String(localized: "Frame ACK suspended by client")
        } else if let depth = state.clientQueueDepth {
            queue = String(format: String(localized: "%lld buffered bytes"), locale: .current, depth)
        } else if state.clientCapabilitiesKnown {
            queue = String(localized: "Frame ACK not received")
        } else {
            queue = String(localized: "Frame ACK pending")
        }
        let frames = state.clientFramesDecoded.map {
            String(format: String(localized: "%lld frames decoded by client"), locale: .current, $0)
        } ?? String(localized: "Decoded frame count pending")
        let timing = qoeTiming(state)
        return [queue, frames, timing].joined(separator: " · ")
    }

    nonisolated private static func qoeTiming(_ state: RdpServer.RdpDiagnostics) -> String {
        guard state.clientQoEReports > 0 else {
            return String(localized: "QoE not reported (optional)")
        }
        guard let milliseconds = state.clientDecodeRenderMilliseconds else {
            return String(localized: "QoE timing unavailable")
        }
        if milliseconds == 0 {
            return String(localized: "Client decode+render 0 ms (or timed out)")
        }
        return String(format: String(localized: "Client decode+render %lld ms"),
                      locale: .current, milliseconds)
    }

    nonisolated private static func clientIdentity(_ state: RdpServer.RdpDiagnostics) -> String {
        guard state.clientRDPVersion != nil || state.clientBuild != nil
                || state.clientConnectionType != nil else {
            return String(localized: "not advertised")
        }
        let version = state.clientRDPVersion.map {
            "RDP \(String(format: "0x%08X", $0))"
        } ?? String(localized: "unknown")
        let build = state.clientBuild.map {
            String(format: String(localized: "build %lld"), locale: .current, $0)
        } ?? String(localized: "build unknown")
        let connection = state.clientConnectionType ?? String(localized: "unknown")
        let requested = if let width = state.clientRequestedWidth,
                            let height = state.clientRequestedHeight {
            "\(width)x\(height)"
        } else {
            String(localized: "unknown")
        }
        let color = state.clientColorDepth ?? String(localized: "unknown")
        let physical: String = if let width = state.clientPhysicalWidthMillimeters,
                                  let height = state.clientPhysicalHeightMillimeters {
            String(localized: "\(width)x\(height) mm")
        } else {
            String(localized: "unknown")
        }
        let orientation = state.clientOrientationDegrees.map {
            String(format: String(localized: "%lld°"), locale: .current, $0)
        } ?? String(localized: "unknown")
        return [
            version,
            build,
            connection,
            "\(String(localized: "requested")) \(requested)",
            "\(String(localized: "color")) \(color)",
            "\(String(localized: "physical")) \(physical)",
            "\(String(localized: "orientation")) \(orientation)"
        ].joined(separator: " · ")
    }

    nonisolated private static func networkPath(_ state: RdpServer.RdpDiagnostics) -> String {
        let rtt = state.networkRTTMilliseconds.map {
            String(format: String(localized: "RTT %lld ms"), locale: .current, $0)
        }
            ?? String(localized: "RTT not measured")
        let baseline = state.networkBaselineRTTMilliseconds.map {
            String(format: String(localized: "baseline RTT %lld ms"), locale: .current, $0)
        } ?? String(localized: "baseline pending")
        let bandwidth = state.networkBandwidthKilobitsPerSecond.map {
            let mbit = Double($0) / 1_000_000
            return String(localized: "measured bandwidth \(mbit, format: .number.precision(.fractionLength(1))) Mbit/s")
        } ?? String(localized: "bandwidth not measured")
        return [rtt, baseline, bandwidth].joined(separator: " · ")
    }

    nonisolated private static func capabilityFlags(_ state: RdpServer.RdpDiagnostics) -> String {
        let early = [
            state.clientEarlyCapabilityFlags & 0x0080 != 0 ? String(localized: "auto-detect (AutoDetect)") : nil,
            state.clientEarlyCapabilityFlags & 0x0100 != 0 ? String(localized: "dynamic graphics (DVC-GFX)") : nil,
            state.clientEarlyCapabilityFlags & 0x0040 != 0 ? String(localized: "monitor layout (MonitorLayout)") : nil,
            state.clientEarlyCapabilityFlags & 0x0400 != 0 ? String(localized: "heartbeat (Heartbeat)") : nil,
            state.clientEarlyCapabilityFlags & 0x0020 != 0
                ? String(localized: "connection type (ConnectionType)") : nil,
            state.clientEarlyCapabilityFlags & 0x0002 != 0 ? String(localized: "32-bit color (32bpp)") : nil
        ].compactMap { $0 }.joined(separator: ", ")
        let info = [
            state.clientInfoFlags & 0x0000_0010 != 0 ? String(localized: "Unicode text") : nil,
            state.clientInfoFlags & 0x0000_0010 == 0 ? String(localized: "non-Unicode text") : nil,
            state.clientInfoFlags & 0x0000_0080 != 0 ? String(localized: "compression") : nil,
            state.clientInfoFlags & 0x0002_0000 != 0 ? String(localized: "mouse wheel") : nil
        ].compactMap { $0 }.joined(separator: ", ")
        let infoFlags = String(state.clientInfoFlags, radix: 16, uppercase: true)
        let earlyFlags = String(state.clientEarlyCapabilityFlags, radix: 16, uppercase: true)
        let performanceFlags = String(state.performanceFlags, radix: 16, uppercase: true)
        let none = String(localized: "none")
        return "\(String(localized: "Info")) 0x\(infoFlags) [\(info.isEmpty ? none : info)] "
            + "\(String(localized: "Early")) 0x\(earlyFlags) [\(early.isEmpty ? none : early)] "
            + "\(String(localized: "Performance")) 0x\(performanceFlags)"
    }

    private struct DiagnosticsFields {
        let heading: String
        let connection: String
        let video: String
        let session: String
        let egfx: String
        let legacy: String
        let transport: String
        let formats: String
    }

    nonisolated private static func diagnosticsFields(
        _ state: RdpServer.RdpDiagnostics,
        connected: Bool,
        initial: Bool,
        width: Int,
        height: Int
    ) -> DiagnosticsFields {
        // Helper values keep the copyable overlay block within one screen of code.
        let session = sessionDescription(state, width: width, height: height)
        let transport = transportDescription(state)
        let egfx = egfxDescription(state)
        let legacy = legacyDescription(state)
        return DiagnosticsFields(
            heading: heading(initial: initial),
            connection: connected
                ? String(localized: "Connected (authentication and RDP session established)")
                : String(localized: "Waiting for client"),
            video: state.width > 0 && state.height > 0
                ? "\(width)x\(height)"
                : String(localized: "negotiating"),
            session: session,
            egfx: egfx,
            legacy: legacy,
            transport: transport,
            formats: formatsDescription(state))
    }

    nonisolated private static func heading(initial: Bool) -> String {
        initial
            ? String(localized: "Test bitmap — waiting for a real video frame")
            : String(localized: "Live video")
    }

    nonisolated private static func sessionDescription(
        _ state: RdpServer.RdpDiagnostics, width: Int, height: Int
    ) -> String {
        let scale = state.scalePercent.map { "\($0)%" } ?? String(localized: "unknown")
        let deviceScale = state.deviceScalePercent.map { "\($0)%" } ?? String(localized: "unknown")
        let monitor = state.monitorCount > 0
            ? "\(state.monitorCount)"
            : (state.clientCapabilitiesKnown ? String(localized: "not advertised") : String(localized: "unknown"))
        return [
            "\(width)x\(height)",
            "\(String(localized: "scale")) \(scale)",
            "\(String(localized: "device")) \(deviceScale)",
            "\(String(localized: "monitors")) \(monitor)"
        ].joined(separator: " · ")
    }

    nonisolated private static func transportDescription(
        _ state: RdpServer.RdpDiagnostics
    ) -> String {
        let udpOffer: String
        let softSync: String
        if !state.clientCapabilitiesKnown {
            udpOffer = String(localized: "pending")
            softSync = String(localized: "pending")
        } else if !state.multitransportOffered {
            udpOffer = String(localized: "not offered")
            softSync = String(localized: "not offered")
        } else {
            udpOffer = state.clientSupportsUDPFECR
                ? String(localized: "supported") : String(localized: "not advertised")
            softSync = state.clientSupportsSoftSync
                ? String(localized: "supported") : String(localized: "not advertised")
        }
        let response: String = switch state.multitransportResponse {
        case .some(true): String(localized: "accepted")
        case .some(false): String(localized: "declined")
        case .none: String(localized: "pending")
        }
        let transport = state.transport == "udp" ? "UDP" : "TCP"
        return [
            transport,
            "UDP/FECR \(udpOffer)",
            "Soft-Sync \(softSync)",
            response
        ].joined(separator: " · ")
    }

    nonisolated private static func egfxDescription(
        _ state: RdpServer.RdpDiagnostics
    ) -> String {
        let version = state.egfxCapabilityVersion.map { String(format: "0x%08X", $0) }
            ?? String(localized: "pending")
        let codec = state.selectedVideoPath ?? String(localized: "pending")
        let channel = state.egfxOpen
            ? "\(String(localized: "open")) #\(state.egfxChannelID.map(String.init) ?? "?")"
            : String(localized: "not open")
        return [
            channel,
            "\(String(localized: "capability")) \(version)",
            "\(String(localized: "selected")) \(codec)"
        ].joined(separator: " · ")
    }

    nonisolated private static func formatsDescription(
        _ state: RdpServer.RdpDiagnostics
    ) -> String {
        let legacyCodecs = [
            state.legacyRemoteFX ? "RemoteFX" : nil,
            state.legacyImageRemoteFX ? "Image RemoteFX" : nil,
            state.legacyNSCodec ? "NSCodec" : nil,
            state.legacyQOI ? "QOI" : nil,
            state.legacyQOIZ ? "QOI-Z" : nil
        ].compactMap { $0 }.joined(separator: ", ")
        return legacyCodecs.isEmpty ? String(localized: "none advertised") : legacyCodecs
    }

    nonisolated private static func legacyDescription(
        _ state: RdpServer.RdpDiagnostics
    ) -> String {
        let selectedLegacy = state.legacySelectedCodec ?? String(localized: "pending")
        return [
            selectedLegacy,
            "\(String(localized: "surface flags")) \(String(format: "0x%08X", state.legacySurfaceCommands))"
        ].joined(separator: " · ")
    }

    nonisolated static func formatAdaptiveVideoStats(ackDelayMs: Double, targetBitrate: Int,
                                                     acknowledgedGoodputBps: Double = 0,
                                                     congested: Bool) -> String {
        let mbit = Double(targetBitrate) / 1_000_000
        let goodput = Double(acknowledgedGoodputBps) / 1_000_000
        let delayText = ackDelayMs > 0
            ? String(localized: "\(ackDelayMs, format: .number.precision(.fractionLength(0))) ms")
            : String(localized: "pending")
        let goodputText = acknowledgedGoodputBps > 0
            ? String(localized: "\(goodput, format: .number.precision(.fractionLength(1))) Mbit/s")
            : String(localized: "pending")
        let congestionText = congested ? " · \(String(localized: "Congested"))" : ""
        let ackText = "\(String(localized: "ACK delay")) \(delayText)"
        let payloadText = "\(String(localized: "Payload goodput")) \(goodputText)"
        let targetText = "\(String(localized: "Target bitrate")) " +
            "\(mbit.formatted(.number.precision(.fractionLength(1)))) Mbit/s"
        return [ackText, payloadText, targetText + congestionText].joined(separator: " · ")
    }

}
