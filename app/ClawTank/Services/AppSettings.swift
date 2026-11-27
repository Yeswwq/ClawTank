// User settings, persisted in UserDefaults. One place for every tunable so the Settings window,
// the menu and the server all agree. Network settings (port, LAN-only, allowlist) take effect by
// restarting the server; the rest apply at the next session or immediately where noted.

import Combine
import Foundation

enum QualityPreset: String, CaseIterable, Identifiable, Sendable {
    // `sharpText` keeps its stored name for existing settings; it's shown as "High quality":
    // measured, extra bit rate does not sharpen text (4:2:0 colour subsampling does; ADR-021).
    case balanced, sharpText, lowBandwidth
    var id: String { rawValue }

    var title: String {
        switch self {
        case .balanced: String(localized: "Balanced")
        case .sharpText: String(localized: "High quality (more bandwidth)")
        case .lowBandwidth: String(localized: "Low bandwidth")
        }
    }

    /// Multiplier on the encoder's default bit rate.
    var bitRateFactor: Double {
        switch self {
        case .balanced: 1.0
        case .sharpText: 1.6
        case .lowBandwidth: 0.35
        }
    }

    var preference: Int {
        switch self {
        case .lowBandwidth: 0
        case .balanced: 50
        case .sharpText: 100
        }
    }

    init(preference: Int) {
        switch preference {
        case ..<34: self = .lowBandwidth
        case 67...: self = .sharpText
        default: self = .balanced
        }
    }
}

enum RustLogLevel: String, CaseIterable, Identifiable, Sendable {
    case standard, error, warn, info, debug, trace
    var id: String { rawValue }

    var title: String {
        switch self {
        case .standard: String(localized: "Standard")
        case .error: String(localized: "Errors only")
        case .warn: String(localized: "Warnings and errors")
        case .info: String(localized: "Information")
        case .debug: String(localized: "Debug")
        case .trace: String(localized: "Trace")
        }
    }

    var filter: String {
        switch self {
        case .standard: "info,clawtank_core=debug,ironrdp_server=debug,ironrdp_egfx=debug"
        case .error: "error"
        case .warn: "warn"
        case .info: "info"
        case .debug: "debug"
        case .trace: "trace"
        }
    }
}

@MainActor
final class AppSettings: ObservableObject {
    private let defaults: UserDefaults

    @Published var displayMode: DisplayMode { didSet { save(displayMode.rawValue, "displayMode") } }
    @Published var virtualDisplayDensity: VirtualDisplayDensity {
        didSet { save(virtualDisplayDensity.rawValue, "virtualDisplayDensity") }
    }
    @Published var maxFrameRate: Int { didSet { save(maxFrameRate, "maxFrameRate") } }
    @Published var quality: QualityPreset {
        didSet {
            save(quality.rawValue, "quality")
            if !isSynchronizingQuality, qualityPreference != quality.preference {
                qualityPreference = quality.preference
            }
        }
    }
    /// Continuous quality preference: 0 prioritizes bandwidth, 100 prioritizes image quality.
    @Published var qualityPreference: Int {
        didSet {
            let clamped = min(100, max(0, qualityPreference))
            if clamped != qualityPreference {
                qualityPreference = clamped
                return
            }
            save(qualityPreference, "qualityPreference")
            let preset = QualityPreset(preference: qualityPreference)
            if quality != preset {
                isSynchronizingQuality = true
                quality = preset
                isSynchronizingQuality = false
            }
        }
    }
    @Published var swapControlCommand: Bool { didSet { save(swapControlCommand, "swapControlCommand") } }
    @Published var showsStatsOverlay: Bool { didSet { save(showsStatsOverlay, "showsStatsOverlay") } }
    @Published var rustLogLevel: RustLogLevel { didSet { save(rustLogLevel.rawValue, "rustLogLevel") } }
    @Published var keepAwake: Bool { didSet { save(keepAwake, "keepAwake") } }
    @Published var port: Int { didSet { save(port, "port") } }
    @Published var lanOnly: Bool { didSet { save(lanOnly, "lanOnly") } }
    /// Comma-separated IPs/CIDRs; empty = anyone allowed by LAN-only.
    @Published var allowlist: String { didSet { save(allowlist, "allowlist") } }
    @Published var hasCompletedOnboarding: Bool { didSet { save(hasCompletedOnboarding, "onboarded") } }
    /// Send the Mac's sound to the client (applies from the next connection).
    @Published var audioEnabled: Bool { didSet { save(audioEnabled, "audioEnabled") } }
    /// Mute the Mac's own speakers while a client is connected.
    @Published var muteLocalAudio: Bool { didSet { save(muteLocalAudio, "muteLocalAudio") } }
    /// Share the clipboard both ways (applies from the next connection).
    @Published var clipboardEnabled: Bool { didSet { save(clipboardEnabled, "clipboardEnabled") } }
    /// Local Agent API (127.0.0.1 only). Off by default.
    @Published var agentAPIEnabled: Bool { didSet { save(agentAPIEnabled, "agentAPIEnabled") } }
    @Published var agentPort: Int { didSet { save(agentPort, "agentPort") } }
    private var isSynchronizingQuality = false

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        displayMode = DisplayMode(rawValue: defaults.string(forKey: "displayMode") ?? "") ?? .automatic
        virtualDisplayDensity = VirtualDisplayDensity(
            rawValue: defaults.string(forKey: "virtualDisplayDensity") ?? "") ?? .clientHint
        let savedFrameRate = defaults.integer(forKey: "maxFrameRate")
        maxFrameRate = [30, 60].contains(savedFrameRate) ? savedFrameRate : 60
        let storedQuality = QualityPreset(rawValue: defaults.string(forKey: "quality") ?? "") ?? .balanced
        quality = storedQuality
        let storedPreference = defaults.object(forKey: "qualityPreference") as? Int ?? storedQuality.preference
        qualityPreference = min(100, max(0, storedPreference))
        swapControlCommand = defaults.bool(forKey: "swapControlCommand")
        showsStatsOverlay = defaults.object(forKey: "showsStatsOverlay") as? Bool ?? true
        rustLogLevel = RustLogLevel(rawValue: defaults.string(forKey: "rustLogLevel") ?? "") ?? .standard
        keepAwake = defaults.object(forKey: "keepAwake") as? Bool ?? true
        let savedPort = defaults.integer(forKey: "port")
        port = (1...65535).contains(savedPort) ? savedPort : 3389
        lanOnly = defaults.object(forKey: "lanOnly") as? Bool ?? true
        allowlist = defaults.string(forKey: "allowlist") ?? ""
        hasCompletedOnboarding = defaults.bool(forKey: "onboarded")
        audioEnabled = defaults.object(forKey: "audioEnabled") as? Bool ?? true
        muteLocalAudio = defaults.bool(forKey: "muteLocalAudio")
        clipboardEnabled = defaults.object(forKey: "clipboardEnabled") as? Bool ?? true
        agentAPIEnabled = defaults.bool(forKey: "agentAPIEnabled")
        let savedAgentPort = defaults.integer(forKey: "agentPort")
        agentPort = (1024...65535).contains(savedAgentPort) ? savedAgentPort : 3390
    }

    /// Frame rate is an independent user limit; quality preference only changes the bitrate budget.
    var frameRate: Int { maxFrameRate }

    /// Encoder target multiplier with exact compatibility anchors for the old presets.
    var bitRateFactor: Double {
        let preference = Double(min(100, max(0, qualityPreference)))
        if preference <= 50 {
            return 0.35 + (1.0 - 0.35) * preference / 50
        }
        return 1.0 + (1.6 - 1.0) * (preference - 50) / 50
    }

    private func save(_ value: Any, _ key: String) {
        defaults.set(value, forKey: key)
    }

    /// Settings for the diagnostics bundle (no secrets are stored here, but keep it explicit).
    var diagnosticsSummary: [String: String] {
        [
            "displayMode": displayMode.rawValue, "virtualDisplayDensity": virtualDisplayDensity.rawValue,
            "maxFrameRate": "\(maxFrameRate)", "quality": quality.rawValue,
            "qualityPreference": "\(qualityPreference)",
            "rustLogLevel": rustLogLevel.rawValue,
            "swapControlCommand": "\(swapControlCommand)", "showsStatsOverlay": "\(showsStatsOverlay)",
            "keepAwake": "\(keepAwake)", "port": "\(port)", "lanOnly": "\(lanOnly)",
            "allowlist": allowlist.isEmpty ? "(none)" : allowlist,
            "audioEnabled": "\(audioEnabled)", "muteLocalAudio": "\(muteLocalAudio)",
            "clipboardEnabled": "\(clipboardEnabled)",
            "agentAPIEnabled": "\(agentAPIEnabled)", "agentPort": "\(agentPort)"
        ]
    }
}
