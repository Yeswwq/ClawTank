//! RDP server lifecycle: build the IronRDP server, run it on a dedicated thread, stop it.
//!
//! We run our own accept loop (mirroring `RdpServer::run` in ironrdp-server 0.13) so that:
//! bind errors are reported synchronously by [`start`]; the real port is known even for
//! port 0; the LAN-only policy runs before any handshake; and stopping works whether or not a
//! client is connected. The server runs on its own small tokio runtime so it never shares
//! threads with the host's capture/encode work. One session at a time (Phase 1).

use std::net::SocketAddr;
use std::thread::JoinHandle;
use std::time::Duration;

use std::sync::Arc;

use ironrdp_pdu::gcc::MultiTransportFlags;
use ironrdp_pdu::rdp::client_info::{ClientInfoFlags, PerformanceFlags};
use ironrdp_server::{
    ConnectionHandler, ConnectionInfo, Credentials, DesktopSize, EgfxTransportObserver, RdpServer,
    ServerError, ServerErrorExt, ServerErrorKind, ServerEvent, ServerResult,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc::UnboundedSender, watch};
use tracing::{error, info, warn};

use crate::Result;
use crate::access::{AccessPolicy, SessionTracker};
use crate::audio::{AudioHub, SoundFactory};
use crate::clipboard::{ClipboardFactory, ClipboardHub};
use crate::display::TestPatternDisplay;
use crate::events::{CoreEvent, EventSink, VideoTransport};
use crate::gfx::GfxFactory;
use crate::input::{InputForwarder, InputSink, TouchRdpeiFactory};
use crate::pointer::PointerHub;
use crate::tls::TlsIdentity;
use crate::video::VideoHub;

struct VideoTransportObserver {
    sink: EventSink,
    transport: watch::Sender<VideoTransport>,
}

impl EgfxTransportObserver for VideoTransportObserver {
    fn on_egfx_udp(&self) {
        let _ = self.transport.send(VideoTransport::Udp);
        (self.sink)(CoreEvent::VideoTransport {
            transport: VideoTransport::Udp,
        });
    }
}

/// Whether a failed connection was rejected by the configured username/password,
/// rather than by TLS, capability exchange, or finalization. The error chain is
/// typed, so this does not depend on an English diagnostic string.
pub(crate) fn credentials_were_rejected(error: &ServerError) -> bool {
    let ServerErrorKind::Connector(connector_error) = error.kind() else {
        return false;
    };
    ironrdp_server::credentials_were_rejected(connector_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector_error(kind: ironrdp_connector::ConnectorErrorKind) -> ServerError {
        ServerError::new(
            "test",
            ironrdp_server::ServerErrorKind::Connector(ironrdp_connector::ConnectorError::new(
                "test", kind,
            )),
        )
    }

    #[test]
    fn only_explicit_credential_rejection_counts_as_authentication_failure() {
        for error_type in [
            ironrdp_connector::sspi::ErrorKind::LogonDenied,
            ironrdp_connector::sspi::ErrorKind::NoCredentials,
        ] {
            let error = connector_error(ironrdp_connector::ConnectorErrorKind::Credssp(
                ironrdp_connector::sspi::Error::new(error_type, "test"),
            ));
            assert!(credentials_were_rejected(&error));
        }

        for kind in [
            ironrdp_connector::ConnectorErrorKind::Decode(ironrdp_core::DecodeError::new(
                "test",
                ironrdp_core::DecodeErrorKind::NotEnoughBytes {
                    received: 0,
                    expected: 4,
                    offset: None,
                },
            )),
            ironrdp_connector::ConnectorErrorKind::General,
            ironrdp_connector::ConnectorErrorKind::AccessDenied,
        ] {
            assert!(!credentials_were_rejected(&connector_error(kind)));
        }
    }
}

struct RdpConnectionDiagnostics {
    video: Arc<VideoHub>,
    sink: EventSink,
    transport: watch::Receiver<VideoTransport>,
    requested_desktop: Option<DesktopSize>,
    client_info_flags: ClientInfoFlags,
    client_early_capability_flags: u32,
    performance_flags: PerformanceFlags,
    desktop_scale: Option<u32>,
    device_scale: Option<u32>,
}

impl RdpConnectionDiagnostics {
    fn new(
        video: Arc<VideoHub>,
        sink: EventSink,
        transport: watch::Receiver<VideoTransport>,
    ) -> Self {
        Self {
            video,
            sink,
            transport,
            requested_desktop: None,
            client_info_flags: ClientInfoFlags::empty(),
            client_early_capability_flags: 0,
            performance_flags: PerformanceFlags::empty(),
            desktop_scale: None,
            device_scale: None,
        }
    }

    fn emit(&self, info: Option<&ConnectionInfo>) {
        let (width, height) = self.video.session_size();
        let desktop_scale = info
            .and_then(|info| info.desktop_scale_factor)
            .or(self.desktop_scale);
        let device_scale = info
            .and_then(|info| info.device_scale_factor)
            .or(self.device_scale);
        let monitor_count = info.map_or(0, |info| info.client_monitors.len());
        let client_info_flags = info
            .map_or(self.client_info_flags, |info| info.client_info_flags)
            .bits();
        let performance_flags = info
            .map_or(self.performance_flags, |info| info.performance_flags)
            .bits();
        let client_early_capability_flags = info
            .map_or(self.client_early_capability_flags, |info| {
                u32::from(info.client_early_capability_flags.bits())
            });
        let multitransport_flags = info
            .map(|info| info.multitransport_flags)
            .unwrap_or(MultiTransportFlags::empty());
        let client_feedback = self.video.client_feedback();
        let client_core = self.video.client_core();
        let network = self.video.network_diagnostics();
        let egfx = self.video.egfx_diagnostics();
        let legacy = self.video.legacy_codecs();

        (self.sink)(CoreEvent::RdpDiagnostics(Box::new(
            crate::events::RdpDiagnosticsEvent {
                width,
                height,
                desktop_scale_percent: desktop_scale,
                device_scale_percent: device_scale,
                client_info_flags,
                client_early_capability_flags,
                performance_flags,
                monitor_count,
                client_capabilities_known: info.is_some(),
                client_supports_udp_fecr: multitransport_flags
                    .contains(MultiTransportFlags::TRANSPORT_TYPE_UDP_FECR),
                client_supports_soft_sync: multitransport_flags
                    .contains(MultiTransportFlags::SOFT_SYNC_TCP_TO_UDP),
                multitransport_offered: info.is_some(),
                multitransport_response: info.and_then(|info| info.multitransport_response_success),
                egfx_channel_open: self.video.channel_open(),
                egfx_channel_id: self.video.channel_id(),
                egfx_capability_versions: egfx.capability_versions,
                egfx_capability_version: egfx.capability_version,
                egfx_avc420: egfx.avc420,
                egfx_avc444: egfx.avc444,
                egfx_progressive: egfx.progressive,
                egfx_bitmap: egfx.bitmap,
                legacy_remote_fx: legacy.remote_fx,
                legacy_image_remote_fx: legacy.image_remote_fx,
                legacy_ns_codec: legacy.ns_codec,
                legacy_qoi: legacy.qoi,
                legacy_qoi_z: legacy.qoi_z,
                legacy_surface_commands: legacy.surface_commands,
                selected_legacy_codec: self.video.selected_legacy_codec(),
                selected_video_path: egfx.selected_path,
                client_queue_depth: client_feedback.queue_depth,
                client_ack_suspended: client_feedback.ack_suspended,
                client_frames_decoded: client_feedback.frames_decoded,
                client_decode_render_ms: client_feedback.decode_render_ms,
                client_qoe_reports: client_feedback.qoe_reports,
                network_rtt_ms: network.rtt_ms,
                network_baseline_rtt_ms: network.baseline_rtt_ms,
                network_bandwidth_kbps: network.bandwidth_kbps,
                client_rdp_version: client_core.as_ref().map(|core| core.rdp_version),
                client_build: client_core.as_ref().map(|core| core.client_build),
                client_requested_desktop: client_core.as_ref().map(|core| core.desktop_size),
                client_color_depth: client_core
                    .as_ref()
                    .map(|core| client_color_depth_label(core.color_depth).to_owned()),
                client_connection_type: client_core
                    .as_ref()
                    .and_then(|core| core.connection_type)
                    .map(|kind| connection_type_label(kind).to_owned()),
                client_physical_size_mm: client_core
                    .as_ref()
                    .and_then(|core| core.physical_size_mm),
                client_orientation_degrees: client_core
                    .as_ref()
                    .and_then(|core| core.orientation_degrees),
                transport: *self.transport.borrow(),
            },
        )));
    }
}

impl ConnectionHandler for RdpConnectionDiagnostics {
    fn on_client_info(&mut self, flags: ClientInfoFlags) -> ServerResult<()> {
        self.client_info_flags = flags;
        info!(
            client_info_flags = flags.bits(),
            "Received RDP Client Info flags"
        );
        Ok(())
    }

    fn on_client_session_config(
        &mut self,
        flags: ClientInfoFlags,
        performance_flags: PerformanceFlags,
        _keyboard_layout: u32,
        _keyboard_type: ironrdp_pdu::gcc::KeyboardType,
        _keyboard_subtype: u32,
        _keyboard_functional_keys_count: u32,
        _ime_file_name: &str,
        desktop_size: DesktopSize,
    ) -> ServerResult<()> {
        if !crate::display::is_supported_desktop_size(desktop_size) {
            warn!(
                width = desktop_size.width,
                height = desktop_size.height,
                "Rejecting initial desktop size outside server display limits"
            );
            return Err(ServerError::reason(
                "client desktop size",
                format!(
                    "{}x{} exceeds the supported display size limits",
                    desktop_size.width, desktop_size.height
                ),
            ));
        }
        info!(
            client_info_flags = flags.bits(),
            performance_flags = performance_flags.bits(),
            offered_width = desktop_size.width,
            offered_height = desktop_size.height,
            "RDP client session configuration received"
        );
        self.performance_flags = performance_flags;
        self.requested_desktop = Some(desktop_size);
        self.video
            .set_session_size(desktop_size.width, desktop_size.height);
        self.emit(None);
        Ok(())
    }

    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        if let Some(client_core) = info.client_core.as_ref() {
            self.video.observe_client_core(client_core);
        }
        self.desktop_scale = info.desktop_scale_factor;
        self.device_scale = info.device_scale_factor;
        self.client_info_flags = info.client_info_flags;
        self.performance_flags = info.performance_flags;
        self.video
            .set_client_scale_percent(info.desktop_scale_factor, info.device_scale_factor);
        let requested = self.requested_desktop.unwrap_or_else(|| {
            let (width, height) = self.video.session_size();
            DesktopSize { width, height }
        });
        info!(
            requested_width = requested.width,
            requested_height = requested.height,
            session_width = self.video.session_size().0,
            session_height = self.video.session_size().1,
            "RDP desktop size selection"
        );
        info!(
            session = info.diagnostic_session_id,
            peer = ?info.peer_addr,
            client_info_flags = info.client_info_flags.bits(),
            performance_flags = info.performance_flags.bits(),
            desktop_scale_factor = ?info.desktop_scale_factor,
            device_scale_factor = ?info.device_scale_factor,
            monitor_count = info.client_monitors.len(),
            auto_reconnect = info.auto_reconnect,
            client_platform = "unknown",
            "RDP connection established; client platform is not identified by protocol metadata"
        );
        (self.sink)(CoreEvent::VideoTransport {
            transport: crate::events::VideoTransport::Tcp,
        });
        self.emit(Some(info));
    }
}

fn client_color_depth_label(depth: ironrdp_pdu::gcc::ClientColorDepth) -> &'static str {
    use ironrdp_pdu::gcc::ClientColorDepth;
    match depth {
        ClientColorDepth::Bpp4 => "4 bpp",
        ClientColorDepth::Bpp8 => "8 bpp",
        ClientColorDepth::Rgb555Bpp16 => "16 bpp RGB555",
        ClientColorDepth::Rgb565Bpp16 => "16 bpp RGB565",
        ClientColorDepth::Bpp24 => "24 bpp",
        ClientColorDepth::Bpp32 => "32 bpp",
    }
}

fn connection_type_label(kind: ironrdp_pdu::gcc::ConnectionType) -> &'static str {
    use ironrdp_pdu::gcc::ConnectionType;
    match kind {
        ConnectionType::NotUsed => "not specified",
        ConnectionType::Modem => "modem",
        ConnectionType::BroadbandLow => "low-bandwidth broadband",
        ConnectionType::Satellite => "satellite",
        ConnectionType::BroadbandHigh => "high-bandwidth broadband",
        ConnectionType::Wan => "WAN",
        ConnectionType::Lan => "LAN",
        ConnectionType::Autodetect => "auto-detect requested",
    }
}

/// Everything needed to start a server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    /// NLA account name (matched exactly; the client's domain, if any, is accepted as sent).
    pub username: String,
    pub password: String,
    pub identity: TlsIdentity,
    /// Who may connect (LAN-only, allowlist); lockout is always on.
    pub policy: AccessPolicy,
}

/// A running server. Dropping it stops the server.
pub struct ServerHandle {
    /// Ends an active session (handled inside IronRDP's session loop).
    quit: UnboundedSender<ServerEvent>,
    /// Ends our accept loop.
    shutdown: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
    local_addr: SocketAddr,
    video: Arc<VideoHub>,
    pointer: Arc<PointerHub>,
    tracker: Arc<SessionTracker>,
    credentials: watch::Sender<Credentials>,
    audio: Arc<AudioHub>,
    clipboard: Arc<ClipboardHub>,
}

impl ServerHandle {
    /// Where the host submits system audio.
    pub fn audio(&self) -> &Arc<AudioHub> {
        &self.audio
    }

    /// Clipboard relay to the client.
    pub fn clipboard(&self) -> &Arc<ClipboardHub> {
        &self.clipboard
    }

    /// Ends the current session, if any; the server keeps listening.
    pub fn disconnect_client(&self) -> bool {
        // Only with a live session: a Quit queued while idle would end the *next* session.
        if !self.tracker.in_session() {
            return false;
        }
        self.quit
            .send(ServerEvent::Quit("disconnected by host".to_owned()))
            .is_ok()
    }

    /// Replaces the NLA credentials; takes effect for the next connection.
    pub fn set_credentials(&self, username: String, password: String) {
        let _ = self.credentials.send(Credentials {
            username,
            password,
            domain: None,
        });
    }

    /// Where the host publishes cursor shapes.
    pub fn pointer(&self) -> &Arc<PointerHub> {
        &self.pointer
    }

    /// Where the host submits encoded video.
    pub fn video(&self) -> &Arc<VideoHub> {
        &self.video
    }

    /// The bound listen address (with the real port if `bind` used port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Disconnects any client, stops listening and waits for the server thread to exit.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.shutdown.send(true);
            let _ = self
                .quit
                .send(ServerEvent::Quit("server stopped by host".to_owned()));
            if thread.join().is_err() {
                error!("RDP server thread panicked");
            }
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the server. Returns once the listener is bound; events arrive on `sink`.
/// Input events arrive on `input`, on a server thread.
pub fn start(config: ServerConfig, sink: EventSink, input: InputSink) -> Result<ServerHandle> {
    start_with_initial_frame(config, sink, input, None)
}

#[doc(hidden)]
pub fn start_with_initial_bgra_frame(
    config: ServerConfig,
    sink: EventSink,
    input: InputSink,
    frame: Vec<u8>,
) -> Result<ServerHandle> {
    start_with_initial_frame(config, sink, input, Some(frame))
}

fn start_with_initial_frame(
    config: ServerConfig,
    sink: EventSink,
    input: InputSink,
    initial_frame: Option<Vec<u8>>,
) -> Result<ServerHandle> {
    let acceptor = config.identity.acceptor()?;
    let public_key = config.identity.public_key()?;

    let listener = std::net::TcpListener::bind(config.bind)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    // UDP and TCP have independent port spaces. Binding EGFX multitransport
    // to the same address keeps one user-facing RDP endpoint while allowing
    // the server to leave the primary session on TCP whenever UDP is
    // unsupported or cannot be established.
    let udp_bind_addr = local_addr;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("clawtank-rdp-worker")
        .enable_all()
        .build()?;

    let tracker = Arc::new(SessionTracker::new(config.policy, sink.clone()));
    let handle_tracker = Arc::clone(&tracker);
    let video = Arc::new(VideoHub::new(sink.clone()));
    let pointer = Arc::new(PointerHub::default());
    let audio = Arc::new(AudioHub::new(sink.clone()));
    let clipboard = Arc::new(ClipboardHub::new(sink.clone()));
    let (thread_audio, thread_clipboard) = (Arc::clone(&audio), Arc::clone(&clipboard));
    let thread_pointer = Arc::clone(&pointer);
    let thread_video = Arc::clone(&video);
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    // RdpServer is not Send, so it is built on the server thread; its quit sender comes back here.
    let (quit_tx, quit_rx) = std::sync::mpsc::sync_channel(1);
    // The latest credentials; applied before each new connection (live password change).
    let (credentials_tx, mut credentials_rx) = watch::channel(Credentials {
        username: config.username,
        password: config.password,
        domain: None,
    });

    let thread = std::thread::Builder::new()
        .name("clawtank-rdp".to_owned())
        .spawn(move || {
            let bitmap_diagnostics = std::env::var_os("CLAWTANK_RDP_BITMAP_DIAGNOSTICS")
                .is_some_and(|value| value == "1");
            ironrdp_server::set_bitmap_diagnostics_enabled(bitmap_diagnostics);
            info!(
                bitmap_diagnostics,
                "RDP first-bitmap metadata diagnostics configured"
            );
            let display = TestPatternDisplay::with_optional_initial_bgra_frame(
                Arc::clone(&tracker),
                Arc::clone(&thread_video),
                thread_pointer,
                initial_frame,
            )
            .with_sink(sink.clone());
            let (transport_tx, transport_rx) = watch::channel(crate::events::VideoTransport::Tcp);
            let mut server = RdpServer::builder()
                .with_addr(local_addr)
                .with_hybrid(acceptor, public_key)
                .with_input_handler(InputForwarder::new(Arc::clone(&input)))
                .with_display_handler(display)
                .with_honor_client_desktop_size(Some(DesktopSize {
                    width: 3840,
                    height: 3840,
                }))
                .with_udp_transport(udp_bind_addr)
                .with_rdpei_factory(Some(Box::new(TouchRdpeiFactory::new(Arc::clone(&input)))))
                .with_connection_handler(Some(Box::new(RdpConnectionDiagnostics::new(
                    Arc::clone(&thread_video),
                    sink.clone(),
                    transport_rx,
                ))))
                .with_gfx_factory(Some(Box::new(GfxFactory::new(Arc::clone(&thread_video)))))
                .with_sound_factory(Some(Box::new(SoundFactory::new(Arc::clone(&thread_audio)))))
                .with_cliprdr_factory(Some(Box::new(ClipboardFactory::new(Arc::clone(
                    &thread_clipboard,
                )))))
                .build();
            server.set_egfx_transport_observer(Some(Arc::new(VideoTransportObserver {
                sink: sink.clone(),
                transport: transport_tx,
            })));
            server.enable_autodetect();
            let server_sender = server.event_sender().clone();
            server.set_credentials(Some(credentials_rx.borrow_and_update().clone()));
            let _ = quit_tx.send(server.event_sender().clone());

            runtime.block_on(async move {
                let mut rtt_interval = tokio::time::interval(Duration::from_secs(2));
                rtt_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                rtt_interval.tick().await;
                let listener = match TcpListener::from_std(listener) {
                    Ok(l) => l,
                    Err(e) => {
                        sink(CoreEvent::Error(format!("listener: {e}")));
                        sink(CoreEvent::Stopped);
                        return;
                    }
                };
                info!(%local_addr, "RDP server listening");
                sink(CoreEvent::Listening(local_addr));

                loop {
                    let (stream, peer) = tokio::select! {
                        _ = rtt_interval.tick() => {
                            if server_sender.send(ServerEvent::AutoDetectRttRequest).is_err() {
                                break;
                            }
                            continue;
                        }
                        _ = shutdown_rx.wait_for(|stop| *stop) => break,
                        accepted = listener.accept() => match accepted {
                            Ok(a) => a,
                            Err(e) => {
                                // Errors like EMFILE repeat immediately; back off instead of spinning.
                                warn!(error = %e, "accept failed");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                continue;
                            }
                        },
                    };
                    if !tracker.accept(peer) {
                        continue;
                    }
                    if credentials_rx.has_changed().unwrap_or(false) {
                        server.set_credentials(Some(credentials_rx.borrow_and_update().clone()));
                        info!("Applied new credentials");
                    }
                    if let Err(e) = stream.set_nodelay(true) {
                        warn!(%peer, error = %e, "TCP_NODELAY failed; latency may suffer");
                    }
                    server.set_connection_peer_addr(Some(peer));
                    if let Ok(local) = stream.local_addr() {
                        server.set_connection_local_addr(Some(local));
                    }
                    let mut connection = Box::pin(server.run_connection(stream));
                    let result = loop {
                        tokio::select! {
                            result = &mut connection => break result,
                            _ = rtt_interval.tick() => {
                                if server_sender.send(ServerEvent::AutoDetectRttRequest).is_err() {
                                    break Err(ServerError::reason(
                                        "auto-detect",
                                        "server event channel closed",
                                    ));
                                }
                            }
                            _ = shutdown_rx.wait_for(|stop| *stop) => {
                                break Err(ServerError::reason("shutdown", "server stopping"));
                            }
                        }
                    };
                    thread_video.session_ended();
                    thread_audio.stop();
                    thread_clipboard.session_ended();
                    let (detail, credentials_rejected) = match result {
                        Ok(()) => (None, false),
                        Err(error) => (
                            Some(error.report().with_locations().to_string()),
                            credentials_were_rejected(&error),
                        ),
                    };
                    tracker.finished(peer, detail, credentials_rejected);
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                info!("RDP server stopped");
                sink(CoreEvent::Stopped);
            });
            // Don't let a stuck session task block process shutdown forever.
            runtime.shutdown_timeout(Duration::from_secs(2));
        })?;
    let quit = quit_rx
        .recv()
        .map_err(|_| "RDP server thread exited during startup")?;

    Ok(ServerHandle {
        quit,
        shutdown,
        thread: Some(thread),
        local_addr,
        video,
        pointer,
        tracker: handle_tracker,
        credentials: credentials_tx,
        audio,
        clipboard,
    })
}
