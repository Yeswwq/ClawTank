import Foundation

/// A Bool readable from any thread (the overlay toggle is read on the capture queue).
final class AtomicFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    func get() -> Bool { lock.withLock { value } }
    func set(_ newValue: Bool) { lock.withLock { value = newValue } }
}

final class AtomicOverlayState: @unchecked Sendable {
    struct Snapshot: Sendable {
        let enabled: Bool
        let text: String
        let fullScreen: Bool
    }

    private struct State {
        var enabled: Bool
        var text: String
        var fullScreen: Bool
    }

    private let lock = NSLock()
    private var state = State(enabled: false, text: "", fullScreen: true)

    func get() -> Snapshot {
        lock.withLock { Snapshot(enabled: state.enabled, text: state.text, fullScreen: state.fullScreen) }
    }

    func set(enabled: Bool) {
        lock.withLock { state.enabled = enabled }
    }

    func set(text: String) {
        lock.withLock { state.text = text }
    }

    func set(fullScreen: Bool) {
        lock.withLock { state.fullScreen = fullScreen }
    }
}

struct VideoStartRequest {
    let width: Int
    let height: Int
    let scalePercent: Int?
    let mode: DisplayMode
    let density: VirtualDisplayDensity
    let frameRate: Int
    let bitRateFactor: Double
    let transport: Transport
    let generation: UInt64

    enum Transport: Equatable {
        case avc
        case progressive
        case bitmap
    }
}

@MainActor
final class PendingVideoStart {
    var request: VideoStartRequest
    init(_ request: VideoStartRequest) { self.request = request }
}
