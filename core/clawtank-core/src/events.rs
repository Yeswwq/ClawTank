//! Events the core reports to the host app (connection lifecycle, server state).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdpDiagnosticsEvent {
    pub width: u16,
    pub height: u16,
    pub desktop_scale_percent: Option<u32>,
    pub device_scale_percent: Option<u32>,
    pub client_info_flags: u32,
    pub client_early_capability_flags: u32,
    pub performance_flags: u32,
    pub monitor_count: usize,
    pub client_capabilities_known: bool,
    pub client_supports_udp_fecr: bool,
    pub client_supports_soft_sync: bool,
    pub multitransport_offered: bool,
    pub multitransport_response: Option<bool>,
    pub egfx_channel_open: bool,
    pub egfx_channel_id: Option<u32>,
    pub egfx_capability_versions: Vec<u32>,
    pub egfx_capability_version: Option<u32>,
    pub egfx_avc420: bool,
    pub egfx_avc444: bool,
    pub egfx_progressive: bool,
    pub egfx_bitmap: bool,
    pub legacy_remote_fx: bool,
    pub legacy_image_remote_fx: bool,
    pub legacy_ns_codec: bool,
    pub legacy_qoi: bool,
    pub legacy_qoi_z: bool,
    pub legacy_surface_commands: u32,
    pub selected_legacy_codec: Option<String>,
    pub selected_video_path: Option<String>,
    /// Bytes buffered by the client, when it reports a concrete value.
    /// QUEUE_DEPTH_UNAVAILABLE and ACK-suspended are represented separately.
    pub client_queue_depth: Option<u32>,
    pub client_ack_suspended: bool,
    /// `None` until the client sends its first EGFX Frame Acknowledge.
    pub client_frames_decoded: Option<u32>,
    /// MS-RDPEGFX QoE ACK decode-to-render duration in milliseconds.
    pub client_decode_render_ms: Option<u16>,
    pub client_qoe_reports: u64,
    pub network_rtt_ms: Option<u32>,
    pub network_baseline_rtt_ms: Option<u32>,
    pub network_bandwidth_kbps: Option<u32>,
    pub client_rdp_version: Option<u32>,
    pub client_build: Option<u32>,
    pub client_requested_desktop: Option<(u16, u16)>,
    pub client_color_depth: Option<String>,
    pub client_connection_type: Option<String>,
    pub client_physical_size_mm: Option<(u32, u32)>,
    pub client_orientation_degrees: Option<u16>,
    pub transport: VideoTransport,
}

/// Something the host app should know about. Delivered on the core's network thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    /// The listener is bound and accepting connections.
    Listening(SocketAddr),
    /// A peer was refused by the access policy before any handshake.
    Rejected {
        peer: SocketAddr,
        reason: RejectReason,
    },
    /// Repeated failed logins: `ip` is refused for `minutes`.
    LockedOut { ip: IpAddr, minutes: u32 },
    /// A client finished TLS + NLA and the session started.
    ClientConnected(SocketAddr),
    /// A connected client's session ended (`error` is `None` for a clean disconnect).
    ClientDisconnected {
        peer: SocketAddr,
        error: Option<String>,
    },
    /// A connection ended before the session started (bad password, TLS failure, …).
    ConnectionFailed {
        peer: SocketAddr,
        error: String,
        /// True only when the configured credentials were explicitly rejected.
        credentials_rejected: bool,
    },
    /// The initial desktop size is known, before the test bitmap is sent. The
    /// host may render a diagnostic image and hand it back through
    /// [`ct_server_submit_initial_bitmap`](crate::ffi::ct_server_submit_initial_bitmap).
    InitialScreen { width: u16, height: u16 },
    /// The client's graphics pipeline is ready: start capturing/encoding at this size.
    VideoStart {
        width: u16,
        height: u16,
        /// Client-reported desktop scale; absent or invalid values are unknown, not 1x.
        scale_percent: Option<u32>,
    },
    /// AVC is disabled but RemoteFX Progressive was confirmed by EGFX; stream captured BGRA
    /// through an EGFX `WireToSurface2` path for this connection.
    ProgressiveStart {
        width: u16,
        height: u16,
        scale_percent: Option<u32>,
    },
    /// Neither AVC nor RemoteFX Progressive is available; stream captured BGRA through the
    /// legacy RDP bitmap update path for this connection.
    BitmapStart {
        width: u16,
        height: u16,
        scale_percent: Option<u32>,
    },
    /// The client resized its desktop: restart capture/encoding at this size.
    /// `scale_percent` is the client's desktop scale (100 = 1x, 200 = Retina).
    VideoResize {
        width: u16,
        height: u16,
        /// A valid client scale hint; absent/invalid layout fields retain the last valid hint.
        scale_percent: Option<u32>,
    },
    /// The RemoteFX Progressive stream's desktop size changed.
    ProgressiveResize {
        width: u16,
        height: u16,
        scale_percent: Option<u32>,
    },
    /// The legacy bitmap stream's desktop size changed.
    BitmapResize {
        width: u16,
        height: u16,
        scale_percent: Option<u32>,
    },
    /// The graphics pipeline closed: stop capturing/encoding.
    VideoStop,
    /// The transport selected for the EGFX data path. The server starts each
    /// session in `Tcp`; `Udp` is sent only after EGFX output has actually
    /// migrated to the RDPEUDP2 + TLS + RDPEMT sideband.
    VideoTransport { transport: VideoTransport },
    /// Structured connection/channel facts for the first-frame diagnostics
    /// overlay. This is intentionally independent of frame statistics: it is
    /// available before EGFX is ready and remains useful when no real frame
    /// has arrived yet.
    RdpDiagnostics(Box<RdpDiagnosticsEvent>),
    /// Client requested display refresh after suppressing output; send a fresh keyframe.
    VideoRefresh,
    /// Audio negotiated: start capturing system audio as 16-bit PCM in this format.
    AudioStart { rate: u32, channels: u16 },
    /// Audio channel closed: stop capturing audio.
    AudioStop,
    /// Clipboard channel up (or the client asked for our formats): announce the host clipboard.
    ClipboardReady,
    /// The client copied: these formats are available (fetch with a request).
    ClipboardRemoteFormats(Vec<crate::clipboard::Format>),
    /// The client wants the host clipboard in `format`; answer with provide.
    ClipboardDataRequested { format: u32 },
    /// Data requested from the client arrived; take it from the hub.
    ClipboardData { format: u32 },
    /// The client copied files/folders (paths sanitized by IronRDP; the host must still check).
    ClipboardRemoteFiles(Vec<crate::clipboard::FileEntry>),
    /// The client wants (part of) file `index` of the host's announced list.
    ClipboardFileRequested {
        stream_id: u32,
        index: u32,
        query: crate::clipboard::FileQuery,
    },
    /// File contents requested from the client arrived for `stream_id`.
    ClipboardFileContents { stream_id: u32 },
    /// The server loop exited; no more events follow.
    Stopped,
    /// The server loop failed.
    Error(String),
}

/// Data-plane transport selected by the RDP server for EGFX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoTransport {
    Tcp,
    Udp,
}

/// Why a peer was refused before the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Not a local/private address and LAN-only is on.
    LanOnly,
    /// An allowlist is configured and the address isn't on it.
    NotAllowlisted,
    /// Too many recent failed logins from this address.
    LockedOut,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LanOnly => "lan-only",
            Self::NotAllowlisted => "allowlist",
            Self::LockedOut => "locked-out",
        }
    }
}

/// Callback receiving [`CoreEvent`]s.
pub type EventSink = Arc<dyn Fn(CoreEvent) + Send + Sync>;
