// Owns the RDP server's lifecycle for the app and publishes its state to the UI.
// Core events arrive on a Rust thread and are applied on the main actor; every connection
// attempt is written to the audit log, and the Mac is kept awake while a client is connected.

import AppKit
import ApplicationServices
import ClawTankCore
import Combine
import Foundation
import os
import SystemConfiguration

@MainActor
final class ServerController: ObservableObject {
    enum Status: Equatable {
        case stopped
        case listening(port: UInt16)
        case connected(peer: String)
        case failed(String)
    }

    struct SessionInfo: Equatable {
        let peer: String
        let since: Date
        var resolution: String?
    }

    @Published private(set) var status: Status = .stopped
    /// Most recent noteworthy event (failed login, refused peer), shown in the menu.
    @Published private(set) var lastNotice: String?
    @Published private(set) var fingerprint = ""
    /// The connected client, if any.
    @Published var session: SessionInfo?
    /// Live video readout, e.g. "60 fps · 14.2 Mbit/s · enc 3.1 ms"; nil when not streaming.
    @Published var videoStatus: String?
    /// e.g. "Virtual display 1512×982" while streaming.
    @Published var displayDescription: String?
    /// Tailscale status: how (and whether) this Mac can be reached from outside the LAN.
    @Published private(set) var remote: TailscaleState = .notInstalled
    @Published private(set) var wakeForNetwork: Bool?
    /// File transfer progress, e.g. "Receiving files: 40% of 1.2 GB".
    @Published private(set) var transferStatus: String?
    /// The Mac's screen is locked: a remote user sees only the wallpaper (macOS hides the lock
    /// screen from capture and blocks synthetic input there; see ADR-018).
    @Published private(set) var screenLocked = false

    let settings: AppSettings
    let audit: AuditLog
    let username: String
    let hostName: String

    private let security: SecurityService
    private let power: PowerManaging
    private(set) var password = ""
    var server: RdpServer?
    var video: VideoSession?
    private var bitmap: BitmapSession?
    private var videoTask: Task<Void, Never>?
    private var pendingVideoStart: PendingVideoStart?
    private var videoGeneration: UInt64 = 0
    private let displays = DisplayService()
    var statsTimer: Timer?
    var lastCounters = VideoSession.Counters()
    var lastAckedBytes: UInt64 = 0
    var lastAckedFrames: UInt64 = 0
    var lastStatsSampleUptime = ProcessInfo.processInfo.systemUptime
    private let overlayFlag = AtomicFlag()
    let overlayState = AtomicOverlayState()
    var rdpDiagnostics = RdpServer.RdpDiagnostics()
    var hasLiveVideoFrame = false
    private let input: InputPipeline
    private var cursor: CursorPublisher?
    private var media: SessionMedia?
    /// Answers file-contents requests on its own queue, straight from the core's thread.
    private let fileServer = FileServer()
    private var subscriptions: Set<AnyCancellable> = []
    let log = Logger(subsystem: "dev.clawtank.ClawTank", category: "server")

    /// RDP input marks a person as active; the Agent API yields to them (ADR-023).
    let humanActivity: HumanActivity

    init(settings: AppSettings, security: SecurityService = SecurityService(), audit: AuditLog = AuditLog(),
         power: PowerManaging = PowerAssertions(), humanActivity: HumanActivity = HumanActivity()) {
        self.settings = settings
        self.humanActivity = humanActivity
        self.security = security
        self.audit = audit
        self.power = power
        self.username = security.username
        self.hostName = Self.localHostName()
        self.input = InputPipeline(swapControlCommand: settings.swapControlCommand)
        overlayFlag.set(settings.showsStatsOverlay)
        overlayState.set(enabled: settings.showsStatsOverlay)
        settings.$showsStatsOverlay.sink { [overlayFlag, overlayState] enabled in
            overlayFlag.set(enabled)
            overlayState.set(enabled: enabled)
        }.store(in: &subscriptions)
        settings.$swapControlCommand.sink { [input] in input.setSwapControlCommand($0) }.store(in: &subscriptions)
        let center = DistributedNotificationCenter.default()
        center.publisher(for: Notification.Name("com.apple.screenIsLocked"))
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in self?.setScreenLocked(true) }.store(in: &subscriptions)
        center.publisher(for: Notification.Name("com.apple.screenIsUnlocked"))
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in self?.setScreenLocked(false) }.store(in: &subscriptions)
        Timer.publish(every: 30, on: .main, in: .common).autoconnect().prepend(Date())
            .sink { [weak self] _ in self?.refreshRemoteAccess() }.store(in: &subscriptions)
        settings.$keepAwake.sink { [weak self] keep in
            guard let self, session != nil else { return }
            if keep { _ = power.begin(reason: "ClawTank remote session") } else { power.end() }
        }.store(in: &subscriptions)
    }

    var isRunning: Bool { server != nil }
    var port: UInt16 { UInt16(clamping: settings.port) }
    var address: String { port == 3389 ? hostName : "\(hostName):\(port)" }

    /// Updates remote-access discovery state while keeping the published setters private.
    func updateRemoteAccess(_ state: TailscaleState, wakeForNetwork: Bool?) {
        remote = state
        self.wakeForNetwork = wakeForNetwork
    }

    func start() {
        guard server == nil else { return }
        do {
            let identity = try security.loadOrCreateIdentity(commonName: hostName)
            password = try security.loadOrCreatePassword()
            fingerprint = identity.fingerprint
            let configuration = RdpServer.Configuration(
                port: port, username: username, password: password, identity: identity,
                lanOnly: settings.lanOnly, allowlist: settings.allowlist)
            let input = self.input
            let fileServer = self.fileServer
            let created = try RdpServer(configuration: configuration, onEvent: { [weak self] event in
                // Hot path during file copies: answer on the file queue, not the main thread.
                if case .clipboardFileRequested(let stream, let index, let query) = event {
                    fileServer.serve(stream: stream, index: index, query: query)
                    return
                }
                Task { @MainActor in self?.handle(event) }
            }, onInput: { [humanActivity] in
                humanActivity.touch()
                input.handle($0)
            })
            fileServer.attach(created)
            server = created
            status = .listening(port: server?.port ?? port)
            lastNotice = nil
            log.info("Server started on port \(self.port) lanOnly=\(self.settings.lanOnly)")
        } catch RdpServer.StartError.failed(let code) where code == CT_STATUS_BIND_FAILED.rawValue {
            fail("Port \(port) is in use or unavailable")
        } catch RdpServer.StartError.failed(let code) where code == CT_STATUS_INVALID_ARGUMENT.rawValue {
            fail("Invalid allowlist (use IPs or CIDRs like 192.168.1.0/24)")
        } catch {
            fail("Could not start: \(error)")
        }
    }

    func stop() {
        endSession()
        server?.stop()
        server = nil
        status = .stopped
    }

    /// Applies changed network settings (port, LAN-only, allowlist).
    func restart() {
        stop()
        start()
    }

    /// Ends the current client's session; the server keeps listening.
    func disconnectClient() {
        _ = server?.disconnectClient()
    }

    /// Validates and sets a new RDP password. Returns the problems if it was rejected.
    func setPassword(_ newPassword: String) -> [PasswordPolicy.Problem] {
        let problems = PasswordPolicy.problems(newPassword, username: username)
        guard problems.isEmpty else { return problems }
        do {
            try security.setPassword(newPassword)
            password = newPassword
            if let server, !server.setCredentials(username: username, password: newPassword) {
                log.error("Core rejected new credentials")
            }
            log.info("RDP password changed")
        } catch {
            lastNotice = String(localized: "Couldn't save the password: \(error.localizedDescription)")
        }
        return []
    }
}
// MARK: - RDP events

@MainActor
extension ServerController {
    private func handle(_ event: RdpServer.Event) {
        guard server != nil else { return }
        if handleTransportEvent(event) { return }
        if handleVideoEvent(event) { return }
        if handleMediaEvent(event) { return }
        handleConnection(event)
    }

    private func handleTransportEvent(_ event: RdpServer.Event) -> Bool {
        switch event {
        case .videoTransport(let transport):
            rdpDiagnostics.transport = transport
            log.info("EGFX data transport: \(transport, privacy: .public)")
            updateOverlayDiagnostics()
            return true
        case .rdpDiagnostics(let diagnostics):
            rdpDiagnostics = diagnostics
            logRdpDiagnostics(diagnostics)
            updateOverlayDiagnostics()
            return true
        default:
            return false
        }
    }

    private func logRdpDiagnostics(_ diagnostics: RdpServer.RdpDiagnostics) {
        let transport = diagnostics.transport
        let path = diagnostics.selectedVideoPath ?? "pending"
        let channel = diagnostics.egfxChannelID.map(String.init) ?? "none"
        let message = "RDP diagnostics: \(diagnostics.width)x\(diagnostics.height) transport=\(transport) "
            + "path=\(path) egfxOpen=\(diagnostics.egfxOpen) channel=\(channel)"
        log.info("\(message, privacy: .public)")
    }

    private func handleVideoEvent(_ event: RdpServer.Event) -> Bool {
        if case .initialScreen(let width, let height) = event {
            renderInitialDiagnostics(width: width, height: height)
            return true
        }
        switch event {
        case .videoStart(let width, let height, let scale):
            startVideo(width: width, height: height, scalePercent: scale)
            return true
        case .videoResize(let width, let height, let scale):
            startVideo(width: width, height: height, scalePercent: scale)
            return true
        case .progressiveStart(let width, let height, let scale):
            startBitmap(width: width, height: height, scalePercent: scale, transport: .progressive)
            return true
        case .progressiveResize(let width, let height, let scale):
            startBitmap(width: width, height: height, scalePercent: scale, transport: .progressive)
            return true
        case .bitmapStart(let width, let height, let scale):
            startBitmap(width: width, height: height, scalePercent: scale)
            return true
        case .bitmapResize(let width, let height, let scale):
            startBitmap(width: width, height: height, scalePercent: scale)
            return true
        case .videoStop:
            stopVideo()
            return true
        case .videoRefresh:
            refreshVideo()
            return true
        default:
            return false
        }
    }

    private func refreshVideo() {
        let generation = videoGeneration
        enqueueVideo { [weak self] in
            guard let self, self.videoGeneration == generation, self.session != nil else { return }
            self.video?.resumeAfterClientRefresh()
            self.bitmap?.refresh()
        }
    }

    private func handleMediaEvent(_ event: RdpServer.Event) -> Bool {
        switch event {
        case .audioStart, .audioStop, .clipboardReady, .clipboardRemoteFormats, .clipboardDataRequested,
             .clipboardData, .clipboardRemoteFiles, .clipboardFileRequested, .clipboardFileContents:
            if let media { media.handle(event) } else if case .clipboardDataRequested = event {
                server?.clipboardProvide(nil)
            }
            return true
        default:
            return false
        }
    }

    private func handleConnection(_ event: RdpServer.Event) {
        switch event {
        case .listening:
            if case .connected = status { return }
            status = .listening(port: server?.port ?? port)
        case .clientConnected(let peer):
            beginSession(peer: Self.ipOnly(peer))
        case .clientDisconnected(let peer, let error):
            record(.disconnected, Self.ipOnly(peer), detail: error)
            endSession()
            status = .listening(port: server?.port ?? port)
        case .connectionFailed, .rejected, .lockedOut:
            handleAccessEvent(event)
        case .error(let message):
            fail(message)
        case .stopped:
            if case .failed = status { return }
            status = .stopped
        default:
            break
        }
    }

    private func record(_ outcome: AuditEntry.Outcome, _ ip: String, detail: String? = nil) {
        // Only sessions prove which account was used; the core never learns the username of a
        // failed attempt, so don't imply one.
        let user: String? = outcome == .connected || outcome == .disconnected ? username : nil
        audit.append(AuditEntry(time: Date(), peer: ip, user: user, outcome: outcome,
                                detail: detail.map(Self.friendlyDetail)))
    }

    private func setScreenLocked(_ locked: Bool) {
        screenLocked = locked
        log.notice("Screen \(locked ? "locked" : "unlocked", privacy: .public)")
    }

    private func beginSession(peer ip: String) {
        videoGeneration &+= 1
        hasLiveVideoFrame = false
        rdpDiagnostics = RdpServer.RdpDiagnostics()
        overlayState.set(fullScreen: true)
        overlayState.set(text: String(localized: "Waiting for client capability negotiation…"))
        status = .connected(peer: ip)
        session = SessionInfo(peer: ip, since: Date())
        lastNotice = nil
        record(.connected, ip)
        if settings.keepAwake { _ = power.begin(reason: "ClawTank remote session") }
        startInput()
        if let server {
            media = SessionMedia(server: server, settings: settings, fileServer: fileServer) { [weak self] in
                self?.transferStatus = $0
            }
        }
    }

    private func endSession() {
        videoGeneration &+= 1
        media?.stop()
        media = nil
        stopInput()
        stopVideo(releaseDisplay: true)
        power.end()
        session = nil
    }

    // MARK: - Input and cursor

    private func startInput() {
        // Shows the system prompt the first time; input is ignored until granted.
        // "AXTrustedCheckOptionPrompt" is the value of kAXTrustedCheckOptionPrompt (a C global).
        let options = ["AXTrustedCheckOptionPrompt": true] as CFDictionary
        if !AXIsProcessTrustedWithOptions(options) {
            lastNotice = String(localized:
                "Grant Accessibility to ClawTank in System Settings to enable keyboard and mouse")
            log.notice("Accessibility not granted; input is ignored")
        }
        guard let server, cursor == nil else { return }
        let input = self.input
        let publisher = CursorPublisher(scale: { input.currentScale() }, send: { server.setPointer($0) })
        publisher.start()
        cursor = publisher
    }

    private func stopInput() {
        input.releaseAll()
        cursor?.stop()
        cursor = nil
    }

    private func fail(_ message: String) {
        log.error("\(message, privacy: .public)")
        endSession()
        server?.stop()
        server = nil
        status = .failed(message)
    }

}

// Video lifecycle (same file, so it keeps access to the controller's private state).
extension ServerController {
    /// Failed logins, refusals and lockouts: shown in the menu and written to the audit log.
    private func handleAccessEvent(_ event: RdpServer.Event) {
        switch event {
        case .connectionFailed(let peer, let classification, let error):
            let ip = Self.ipOnly(peer)
            log.notice("Connection from \(ip, privacy: .public) failed: \(error, privacy: .public)")
            lastNotice = Self.describeFailure(error, ip: ip)
            record(Self.connectionFailureOutcome(classification, error), ip, detail: error)
        case .rejected(let peer, let reason):
            let ip = Self.ipOnly(peer)
            lastNotice = Self.describeRefusal(reason, ip: ip)
            record(.refused, ip, detail: reason)
        case .lockedOut(let ip, let minutes):
            lastNotice = String(localized: "Locked out \(ip) for \(minutes) min after repeated failed logins")
            record(.lockedOut, ip, detail: "\(minutes) min")
        default:
            break
        }
    }

    /// Starts (or, after a resize, restarts) video at `width`×`height`. Start, stop and restart
    /// run strictly in order on `videoTask` so a resize can't race a disconnect.
    private func startVideo(width: Int, height: Int, scalePercent: Int?) {
        startSession(width: width, height: height, scalePercent: scalePercent, transport: .avc)
    }

    private func startBitmap(width: Int, height: Int, scalePercent: Int?) {
        startBitmap(width: width, height: height, scalePercent: scalePercent, transport: .bitmap)
    }

    private func startBitmap(width: Int, height: Int, scalePercent: Int?,
                             transport: VideoStartRequest.Transport) {
        startSession(width: width, height: height, scalePercent: scalePercent, transport: transport)
    }

    private func startSession(width: Int, height: Int, scalePercent: Int?,
                              transport: VideoStartRequest.Transport) {
        guard server != nil, session != nil else {
            log.notice("Ignoring display start without an active client session")
            return
        }
        guard CGPreflightScreenCaptureAccess() else {
            // Shows the system prompt the first time; afterwards the user must use Settings.
            CGRequestScreenCaptureAccess()
            lastNotice = String(localized:
                "Grant Screen Recording to ClawTank in System Settings, then reconnect")
            log.notice("Screen Recording not granted; client sees the test pattern")
            return
        }
        let mode = settings.displayMode
        let density = settings.virtualDisplayDensity
        let frameRate = settings.frameRate
        let bitRateFactor = settings.bitRateFactor
        let generation = videoGeneration
        let request = VideoStartRequest(width: width, height: height, scalePercent: scalePercent,
                                        mode: mode, density: density, frameRate: frameRate,
                                        bitRateFactor: bitRateFactor,
                                        transport: transport,
                                        generation: generation)
        overlayState.set(fullScreen: hasLiveVideoFrame ? false : true)
        updateOverlayDiagnostics()
        enqueueVideoStart(request)
    }

    /// Coalesces requests that are queued behind current video work. Once a resize starts, later
    /// requests form a new batch after it, preserving the serial display/encoder lifecycle.
    private func enqueueVideoStart(_ request: VideoStartRequest) {
        if let pendingVideoStart {
            pendingVideoStart.request = request
            log.debug("Coalesced pending video layout to \(request.width)x\(request.height)")
            return
        }

        let pending = PendingVideoStart(request)
        pendingVideoStart = pending
        enqueueVideo { [self] in
            guard pendingVideoStart === pending else {
                log.debug("Discarding queued video layout superseded by session stop")
                return
            }
            pendingVideoStart = nil
            await startVideoSession(pending.request)
        }
    }

    private func startVideoSession(_ request: VideoStartRequest) async {
        let (width, height) = (request.width, request.height)
        await stopCurrentSessions()
        guard videoGeneration == request.generation, session != nil, let server else {
            log.debug("Ignoring stale queued video start")
            return
        }
        guard let target = await displays.prepare(width: width, height: height,
                                                  scalePercent: request.scalePercent, mode: request.mode,
                                                  density: request.density) else {
            lastNotice = String(localized: "No display available to share")
            return
        }
        guard videoGeneration == request.generation, session != nil else {
            log.debug("Discarding display prepared for an ended RDP session")
            return
        }
        input.setSession(width: width, height: height, displayBounds: target.bounds)
        lastCounters = .init()
        let videoStats = server.videoStats
        lastAckedBytes = videoStats.bytes_acked
        lastAckedFrames = videoStats.frames_acked
        lastStatsSampleUptime = ProcessInfo.processInfo.systemUptime
        do {
            try await startCapture(request, target: target, server: server)
            guard videoGeneration == request.generation, session != nil else {
                log.debug("Stopping display startup completed for an ended RDP session")
                return
            }
            publishStartedSession(request, target: target)
        } catch {
            log.error("Display start failed: \(error.localizedDescription, privacy: .public)")
            if request.transport == .avc { self.video = nil } else { self.bitmap = nil }
            lastNotice = String(localized: "Video failed: \(error.localizedDescription)")
            return
        }
        statsTimer?.invalidate()
        statsTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.updateVideoStats() }
        }
    }

    private func startCapture(_ request: VideoStartRequest, target: DisplayTarget,
                              server: RdpServer) async throws {
        let onFirstFrame: @Sendable () -> Void = { [weak self] in
            Task { @MainActor in self?.markLiveVideo() }
        }
        switch request.transport {
        case .avc:
            let flag = overlayFlag
            let overlayState = self.overlayState
            let video = VideoSession(server: server, width: request.width, height: request.height,
                                     target: target, frameRate: request.frameRate,
                                     bitRateFactor: request.bitRateFactor,
                                     overlayEnabled: { flag.get() },
                                     overlayState: overlayState,
                                     onFirstFrameSent: onFirstFrame)
            self.video = video
            do { try await video.start() } catch { self.video = nil; throw error }
        case .bitmap:
            let bitmap = BitmapSession(server: server, width: request.width, height: request.height,
                                       target: target, transport: .legacyBitmap,
                                       onFirstFrameSent: onFirstFrame)
            self.bitmap = bitmap
            do { try await bitmap.start() } catch { self.bitmap = nil; throw error }
        case .progressive:
            let bitmap = BitmapSession(server: server, width: request.width, height: request.height,
                                       target: target, transport: .progressive,
                                       onFirstFrameSent: onFirstFrame)
            self.bitmap = bitmap
            do { try await bitmap.start() } catch { self.bitmap = nil; throw error }
        }
    }

    /// Stops video; with `releaseDisplay` also removes the virtual display (session over).
    private func stopVideo(releaseDisplay: Bool = false) {
        pendingVideoStart = nil
        enqueueVideo { [self] in
            await stopCurrentSessions()
            if releaseDisplay {
                displays.release()
                displayDescription = nil
            }
        }
    }

    private func enqueueVideo(_ operation: @escaping @MainActor () async -> Void) {
        let previous = videoTask
        videoTask = Task { @MainActor in
            await previous?.value
            await operation()
        }
    }

    func stopCurrentBitmap() async {
        guard let bitmap else { return }
        self.bitmap = nil
        await bitmap.stop()
    }

    func stopCurrentSessions() async {
        await stopCurrentVideo()
        await stopCurrentBitmap()
    }
}
