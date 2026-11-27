//! Bridge between the host's H.264 encoder and the EGFX graphics pipeline (MS-RDPEGFX AVC420).
//!
//! The host (VideoToolbox) submits Annex-B access units with [`VideoHub::submit_h264`]. We
//! create the EGFX surface lazily on the first keyframe, queue the frame on IronRDP's
//! `GraphicsPipelineServer`, drain its output and post it to the session loop.
//!
//! Two rules keep the decoder healthy:
//! - A frame that can't be sent (backpressure) is dropped, and every following frame is
//!   refused until the host sends a keyframe. A dropped P-frame would break the reference
//!   chain.
//! - The submit path locks `inner` before the pipeline. IronRDP invokes our handler while holding
//!   the pipeline mutex, so ready/close callbacks never acquire `inner`; event publication uses a
//!   separate lock and only reads atomic layout state. ACK callbacks touch only the independent,
//!   bounded timing table.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_pdu::gcc::{ClientColorDepth, ClientCoreData, ConnectionType, Monitor, MonitorFlags};
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use ironrdp_svc::ChannelFlags;
use tokio::sync::{mpsc::UnboundedSender, watch};
use tracing::{debug, info, warn};

use crate::events::{CoreEvent, EventSink};
use crate::progressive::encode_bgra_frame;

/// Outcome of [`VideoHub::submit_h264`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitResult {
    /// Queued to the client.
    Sent,
    /// No EGFX session yet (or it closed); nothing was sent and nothing is owed.
    NotReady,
    /// Not sent; the host must send a keyframe next.
    NeedKeyframe,
}

/// Counters for the stats readout. All monotonic within a session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VideoStats {
    /// Encoded frames successfully queued to the session loop.
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub frames_dropped: u64,
    pub frames_acked: u64,
    /// Payload bytes belonging to frames acknowledged by the client.
    pub bytes_acked: u64,
    pub frames_in_flight: u32,
    /// Smoothed time from queueing a frame to the client acknowledging it (decode included),
    /// in microseconds; 0 until the first ack. The main congestion signal for adaptive bitrate.
    pub ack_latency_us: u32,
}

/// Shared between the EGFX factory/handler (IronRDP side) and the FFI (host side).
pub struct VideoHub {
    sink: EventSink,
    inner: Mutex<Inner>,
    event_state: Mutex<VideoEventState>,
    // Touched from handler callbacks (pipeline mutex held): atomics or independent callback state.
    channel_open: AtomicBool,
    avc420_supported: AtomicBool,
    progressive_supported: AtomicBool,
    bitmap_supported: AtomicBool,
    display_suppressed: AtomicBool,
    suppression_monitor_generation: AtomicU64,
    session_size: AtomicU32,
    client_scale_percent: AtomicU32,
    frames_acked: AtomicU64,
    bytes_acked: AtomicU64,
    /// Bounded send-time samples, independent of the pipeline and inner locks when callbacks run.
    send_times: Mutex<PendingAckTimes>,
    ack_latency_us: AtomicU32,
    bitmap_mailbox: watch::Sender<BitmapMailbox>,
    bitmap_sequence: AtomicU64,
    initial_pattern_sent: AtomicBool,
    initial_frame_tx: Mutex<Option<tokio::sync::oneshot::Sender<Vec<u8>>>>,
    initial_frame_rx: Mutex<Option<tokio::sync::oneshot::Receiver<Vec<u8>>>>,
    egfx_path: AtomicU8,
    egfx_capability_versions: Mutex<Vec<u32>>,
    egfx_capability_version: AtomicU32,
    egfx_avc420_capability: AtomicBool,
    egfx_avc444_capability: AtomicBool,
    egfx_progressive_capability: AtomicBool,
    egfx_bitmap_capability: AtomicBool,
    legacy_codecs: Mutex<LegacyCodecCapabilities>,
    client_queue_depth: AtomicU32,
    client_ack_received: AtomicBool,
    client_ack_suspended: AtomicBool,
    client_frames_decoded: AtomicU32,
    client_decode_render_ms: AtomicU32,
    client_qoe_reports: AtomicU64,
    network: Mutex<Option<ironrdp_server::autodetect::AutoDetectHandles>>,
    client_core: Mutex<Option<ClientCoreSummary>>,
}

#[derive(Debug, Clone)]
pub(crate) struct BitmapFrame {
    pub sequence: u64,
    pub data: Bytes,
    /// Monotonic enqueue time used only for queue-latency diagnostics.
    pub queued_at: Instant,
}

#[derive(Debug, Clone)]
pub(crate) struct BitmapMailbox {
    pub size: (u16, u16),
    pub frame: Option<BitmapFrame>,
}

#[derive(Default)]
struct VideoEventState {
    announced_layout: Option<(u16, u16, Option<u32>)>,
}

const MAX_PENDING_ACK_TIMES: usize = 256;

#[derive(Default)]
struct PendingAckTimes {
    by_frame: HashMap<u32, (Instant, u64)>,
    order: VecDeque<u32>,
}

impl PendingAckTimes {
    fn insert(&mut self, frame_id: u32, sent_at: Instant, bytes: u64) {
        self.remove(frame_id);
        if self.by_frame.len() >= MAX_PENDING_ACK_TIMES
            && let Some(oldest) = self.order.pop_front()
        {
            self.by_frame.remove(&oldest);
        }
        self.by_frame.insert(frame_id, (sent_at, bytes));
        self.order.push_back(frame_id);
    }

    fn remove(&mut self, frame_id: u32) -> Option<(Instant, u64)> {
        let sample = self.by_frame.remove(&frame_id)?;
        self.order.retain(|pending_id| *pending_id != frame_id);
        Some(sample)
    }

    fn clear(&mut self) {
        self.by_frame.clear();
        self.order.clear();
    }

    fn len(&self) -> usize {
        self.by_frame.len()
    }
}

struct Inner {
    handle: Option<GfxServerHandle>,
    sender: Option<UnboundedSender<ServerEvent>>,
    suppression_source: Option<Arc<AtomicBool>>,
    session_id: u64,
    surface: Option<u16>,
    need_keyframe: bool,
    started: Instant,
    frames_sent: u64,
    bytes_sent: u64,
    frames_dropped: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LegacyCodecCapabilities {
    pub remote_fx: bool,
    pub image_remote_fx: bool,
    pub ns_codec: bool,
    pub qoi: bool,
    pub qoi_z: bool,
    pub surface_commands: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EgfxDiagnostics {
    pub capability_versions: Vec<u32>,
    pub capability_version: Option<u32>,
    pub avc420: bool,
    pub avc444: bool,
    pub progressive: bool,
    pub bitmap: bool,
    pub selected_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientCoreSummary {
    pub rdp_version: u32,
    pub client_build: u32,
    pub desktop_size: (u16, u16),
    pub color_depth: ClientColorDepth,
    pub connection_type: Option<ConnectionType>,
    pub physical_size_mm: Option<(u32, u32)>,
    pub orientation_degrees: Option<u16>,
}

impl ClientCoreSummary {
    fn from_core(core: &ClientCoreData) -> Self {
        let physical_size_mm = core
            .optional_data
            .desktop_physical_width
            .zip(core.optional_data.desktop_physical_height)
            .filter(|(width, height)| {
                (10..=10_000).contains(width) && (10..=10_000).contains(height)
            });
        let orientation_degrees = core
            .optional_data
            .desktop_orientation
            .filter(|orientation| matches!(orientation, 0 | 90 | 180 | 270));
        Self {
            rdp_version: core.version.0,
            client_build: core.client_build,
            desktop_size: (core.desktop_width, core.desktop_height),
            color_depth: core.client_color_depth(),
            connection_type: core.optional_data.connection_type,
            physical_size_mm,
            orientation_degrees,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClientFeedback {
    pub queue_depth: Option<u32>,
    pub ack_suspended: bool,
    pub frames_decoded: Option<u32>,
    pub decode_render_ms: Option<u16>,
    pub qoe_reports: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct NetworkDiagnostics {
    pub rtt_ms: Option<u32>,
    pub baseline_rtt_ms: Option<u32>,
    pub bandwidth_kbps: Option<u32>,
}

const PATH_NONE: u8 = 0;
const PATH_AVC420: u8 = 1;
const PATH_PROGRESSIVE: u8 = 2;
const PATH_BITMAP: u8 = 3;

fn pack(width: u16, height: u16) -> u32 {
    (u32::from(width) << 16) | u32::from(height)
}

fn unpack(v: u32) -> (u16, u16) {
    ((v >> 16) as u16, (v & 0xffff) as u16)
}

fn h264_nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::with_capacity(8);
    let mut cursor = 0;
    while cursor + 3 < data.len() && types.len() < 8 {
        let start = if data[cursor..].starts_with(&[0, 0, 0, 1]) {
            cursor + 4
        } else if data[cursor..].starts_with(&[0, 0, 1]) {
            cursor + 3
        } else {
            cursor += 1;
            continue;
        };
        if let Some(header) = data.get(start) {
            types.push(header & 0x1f);
        }
        cursor = start;
    }
    types
}

impl VideoHub {
    pub fn new(sink: EventSink) -> Self {
        let (bitmap_mailbox, _) = watch::channel(BitmapMailbox {
            size: (1920, 1080),
            frame: None,
        });
        Self {
            sink,
            inner: Mutex::new(Inner {
                handle: None,
                sender: None,
                suppression_source: None,
                session_id: 0,
                surface: None,
                need_keyframe: true,
                started: Instant::now(),
                frames_sent: 0,
                bytes_sent: 0,
                frames_dropped: 0,
            }),
            event_state: Mutex::new(VideoEventState::default()),
            channel_open: AtomicBool::new(false),
            avc420_supported: AtomicBool::new(false),
            progressive_supported: AtomicBool::new(false),
            bitmap_supported: AtomicBool::new(false),
            display_suppressed: AtomicBool::new(false),
            suppression_monitor_generation: AtomicU64::new(0),
            session_size: AtomicU32::new(pack(1920, 1080)),
            client_scale_percent: AtomicU32::new(0),
            frames_acked: AtomicU64::new(0),
            bytes_acked: AtomicU64::new(0),
            send_times: Mutex::new(PendingAckTimes::default()),
            ack_latency_us: AtomicU32::new(0),
            bitmap_mailbox,
            bitmap_sequence: AtomicU64::new(0),
            initial_pattern_sent: AtomicBool::new(false),
            initial_frame_tx: Mutex::new(None),
            initial_frame_rx: Mutex::new(None),
            egfx_path: AtomicU8::new(PATH_NONE),
            egfx_capability_versions: Mutex::new(Vec::new()),
            egfx_capability_version: AtomicU32::new(0),
            egfx_avc420_capability: AtomicBool::new(false),
            egfx_avc444_capability: AtomicBool::new(false),
            egfx_progressive_capability: AtomicBool::new(false),
            egfx_bitmap_capability: AtomicBool::new(false),
            legacy_codecs: Mutex::new(LegacyCodecCapabilities::default()),
            client_queue_depth: AtomicU32::new(0),
            client_ack_received: AtomicBool::new(false),
            client_ack_suspended: AtomicBool::new(false),
            client_frames_decoded: AtomicU32::new(0),
            client_decode_render_ms: AtomicU32::new(0),
            client_qoe_reports: AtomicU64::new(0),
            network: Mutex::new(None),
            client_core: Mutex::new(None),
        }
    }

    pub(crate) fn subscribe_bitmap(&self) -> watch::Receiver<BitmapMailbox> {
        self.bitmap_mailbox.subscribe()
    }

    pub(crate) fn take_initial_test_pattern(&self) -> bool {
        !self.initial_pattern_sent.swap(true, Ordering::AcqRel)
    }

    pub(crate) async fn take_initial_frame(&self) -> Option<Vec<u8>> {
        let receiver = self
            .initial_frame_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        tokio::time::timeout(Duration::from_millis(800), receiver)
            .await
            .ok()
            .and_then(Result::ok)
    }

    /// Replaces the initial test bitmap when Swift has rendered the diagnostic
    /// overlay. The one-shot is intentionally consumed: later connections must
    /// wait for their own initial-screen event.
    pub fn submit_initial_bitmap(&self, width: u16, height: u16, data: Vec<u8>) -> bool {
        if self.session_size() != (width, height)
            || data.len() != usize::from(width) * usize::from(height) * 4
        {
            return false;
        }
        self.initial_frame_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some_and(|sender| sender.send(data).is_ok())
    }

    pub(crate) fn prepare_initial_frame(&self) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        *self
            .initial_frame_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(sender);
        *self
            .initial_frame_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(receiver);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The session loop's event sender (set once when the server is built).
    pub fn set_sender(&self, sender: UnboundedSender<ServerEvent>) {
        self.lock().sender = Some(sender);
    }

    /// Tracks IronRDP's per-connection SuppressOutput state independently of display changes.
    /// ScreenCaptureKit may not produce another sample while the desktop is static.
    pub fn observe_display_suppression(self: &Arc<Self>, source: Arc<AtomicBool>) {
        let generation = self
            .suppression_monitor_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.display_suppressed
            .store(source.load(Ordering::Acquire), Ordering::Release);
        self.lock().suppression_source = Some(Arc::clone(&source));
        let video = Arc::downgrade(self);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(20));
            loop {
                interval.tick().await;
                let Some(video) = video.upgrade() else {
                    break;
                };
                if video.suppression_monitor_generation.load(Ordering::Acquire) != generation {
                    break;
                }
                let suppressed = source.load(Ordering::Acquire);
                let currently_suppressed = video.display_suppressed.load(Ordering::Acquire);
                if suppressed != currently_suppressed {
                    video.update_display_suppression(suppressed);
                }
            }
        });
    }

    fn update_display_suppression(&self, suppressed: bool) {
        let was_suppressed = self.display_suppressed.swap(suppressed, Ordering::AcqRel);
        if was_suppressed == suppressed {
            return;
        }
        if suppressed {
            info!("RDP client output suppressed; pausing video submissions");
            return;
        }

        let mut inner = self.lock();
        inner.need_keyframe = true;
        let output_ready = inner.surface.is_some()
            || self.progressive_supported.load(Ordering::Acquire)
            || self.bitmap_supported.load(Ordering::Acquire);
        let session =
            (self.channel_open.load(Ordering::Acquire) && output_ready).then_some(inner.session_id);
        drop(inner);
        if let Some(session) = session {
            info!(
                session,
                "RDP display output resumed; requesting a fresh frame"
            );
            (self.sink)(CoreEvent::VideoRefresh);
        }
    }

    /// Desktop size negotiated for the current session; the EGFX surface uses it.
    pub fn set_session_size(&self, width: u16, height: u16) {
        let _inner = self.lock();
        self.session_size
            .store(pack(width, height), Ordering::Release);
        self.bitmap_mailbox.send_modify(|mailbox| {
            mailbox.size = (width, height);
            mailbox.frame = None;
        });
    }

    pub fn session_size(&self) -> (u16, u16) {
        unpack(self.session_size.load(Ordering::Acquire))
    }

    /// Stable identifier for the currently attached graphics session, used to
    /// correlate capability and frame diagnostics across reconnects.
    pub(crate) fn session_id(&self) -> u64 {
        self.lock().session_id
    }

    /// Client desktop scale is valid only when its paired device scale uses an RDP-defined value.
    pub fn set_client_scale_percent(&self, desktop_scale: Option<u32>, device_scale: Option<u32>) {
        let value = valid_client_scale_percent(desktop_scale, device_scale).unwrap_or(0);
        self.client_scale_percent.store(value, Ordering::Release);
    }

    pub fn client_scale_percent(&self) -> Option<u32> {
        match self.client_scale_percent.load(Ordering::Acquire) {
            0 => None,
            value => Some(value),
        }
    }

    /// A new connection built its pipeline: forget the previous session's state.
    pub fn attach(&self, handle: GfxServerHandle) {
        let mut inner = self.lock();
        self.channel_open.store(false, Ordering::Release);
        self.avc420_supported.store(false, Ordering::Release);
        self.progressive_supported.store(false, Ordering::Release);
        self.bitmap_supported.store(false, Ordering::Release);
        inner.session_id = inner.session_id.wrapping_add(1).max(1);
        inner.handle = Some(handle);
        inner.surface = None;
        inner.need_keyframe = true;
        inner.started = Instant::now();
        inner.frames_sent = 0;
        inner.bytes_sent = 0;
        inner.frames_dropped = 0;
        self.frames_acked.store(0, Ordering::Release);
        self.bytes_acked.store(0, Ordering::Release);
        self.ack_latency_us.store(0, Ordering::Release);
        self.send_times
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        info!(session = inner.session_id, "EGFX graphics session attached");
    }

    /// The client asked for a new desktop size (Display Control). The next keyframe at that size
    /// recreates the surface (ResetGraphics); frames still arriving at the old size are refused.
    pub fn request_resize(
        &self,
        width: u16,
        height: u16,
        desktop_scale: Option<u32>,
        device_scale: Option<u32>,
    ) {
        let scale_percent = valid_client_scale_percent(desktop_scale, device_scale);
        let size_changed = {
            let mut inner = self.lock();
            let size_changed = self.session_size() != (width, height);
            let scale_changed = self.client_scale_percent()
                != scale_percent.or_else(|| self.client_scale_percent());
            if !size_changed && !scale_changed {
                return;
            }
            if let Some(scale_percent) = scale_percent {
                self.client_scale_percent
                    .store(scale_percent, Ordering::Release);
            }
            if size_changed {
                // Serialize the advertised size with surface invalidation. A submitter either
                // queues against the old surface before this lock, or observes the new size.
                self.session_size
                    .store(pack(width, height), Ordering::Release);
                self.bitmap_mailbox.send_modify(|mailbox| {
                    mailbox.size = (width, height);
                    mailbox.frame = None;
                });
                inner.surface = None;
                inner.need_keyframe = true;
                // Keep invalidation inside the layout critical section. A submitter cannot
                // register a new-size frame's ACK sample between the layout commit and clear.
                self.send_times
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
            }
            size_changed
        };
        let mut event_state = self.event_state.lock().unwrap_or_else(|e| e.into_inner());
        if self.channel_open.load(Ordering::Acquire)
            && (self.avc420_supported.load(Ordering::Acquire)
                || self.progressive_supported.load(Ordering::Acquire)
                || self.bitmap_supported.load(Ordering::Acquire))
        {
            let (width, height) = self.session_size();
            let scale_percent = self.client_scale_percent();
            let layout = (width, height, scale_percent);
            if event_state.announced_layout == Some(layout) {
                return;
            }
            info!(
                width,
                height,
                ?scale_percent,
                size_changed,
                "Applying client display layout"
            );
            let event = if self.avc420_supported.load(Ordering::Acquire) {
                CoreEvent::VideoResize {
                    width,
                    height,
                    scale_percent,
                }
            } else if self.progressive_supported.load(Ordering::Acquire) {
                CoreEvent::ProgressiveResize {
                    width,
                    height,
                    scale_percent,
                }
            } else {
                CoreEvent::BitmapResize {
                    width,
                    height,
                    scale_percent,
                }
            };
            (self.sink)(event);
            event_state.announced_layout = Some(layout);
        }
    }

    /// Handler callback: capabilities negotiated, frames may flow.
    pub(crate) fn on_ready(&self, supports_avc420: bool, supports_remotefx_progressive: bool) {
        let mut event_state = self.event_state.lock().unwrap_or_else(|e| e.into_inner());
        // DisplayControl can set the suppression flag before capabilities finish; each
        // graphics handler replaces the previous connection's shared state.
        self.display_suppressed.store(false, Ordering::Release);
        self.avc420_supported
            .store(supports_avc420, Ordering::Release);
        self.progressive_supported.store(
            !supports_avc420 && supports_remotefx_progressive,
            Ordering::Release,
        );
        self.bitmap_supported.store(
            !supports_avc420 && !supports_remotefx_progressive,
            Ordering::Release,
        );
        if !self.channel_open.swap(true, Ordering::AcqRel) {
            let (w, h) = self.session_size();
            let scale_percent = self.client_scale_percent();
            if supports_avc420 {
                event_state.announced_layout = Some((w, h, scale_percent));
                info!(
                    width = w,
                    height = h,
                    ?scale_percent,
                    codec = "AVC420",
                    "EGFX ready; requesting video"
                );
                (self.sink)(CoreEvent::VideoStart {
                    width: w,
                    height: h,
                    scale_percent,
                });
            } else {
                event_state.announced_layout = Some((w, h, scale_percent));
                info!(
                    width = w,
                    height = h,
                    codec = if supports_remotefx_progressive {
                        "RemoteFXProgressive"
                    } else {
                        "LegacyBitmap"
                    },
                    "EGFX ready without AVC420; selecting compatible capture path"
                );
                if self.progressive_supported.load(Ordering::Acquire) {
                    (self.sink)(CoreEvent::ProgressiveStart {
                        width: w,
                        height: h,
                        scale_percent,
                    });
                } else {
                    (self.sink)(CoreEvent::BitmapStart {
                        width: w,
                        height: h,
                        scale_percent,
                    });
                }
            }
        }
    }

    /// Handler callback: the client acknowledged a frame.
    pub(crate) fn on_frame_ack(&self, frame_id: u32, queue_depth: u32, total_frames_decoded: u32) {
        self.client_ack_received.store(true, Ordering::Release);
        match queue_depth {
            0 => {
                self.client_ack_suspended.store(false, Ordering::Release);
            }
            u32::MAX => {
                self.client_ack_suspended.store(true, Ordering::Release);
            }
            value => {
                self.client_ack_suspended.store(false, Ordering::Release);
                self.client_queue_depth.store(value, Ordering::Release);
            }
        }
        self.client_frames_decoded
            .store(total_frames_decoded, Ordering::Release);
        let mut times = self.send_times.lock().unwrap_or_else(|e| e.into_inner());
        let sample = times.remove(frame_id);
        if let Some((sent_at, payload_bytes)) = sample {
            // Keep the ACK counter in the same critical section as removal so stats snapshots
            // cannot observe one side of the update without the other.
            let acked = self.frames_acked.fetch_add(1, Ordering::AcqRel) + 1;
            self.bytes_acked.fetch_add(payload_bytes, Ordering::AcqRel);
            let sample = u32::try_from(sent_at.elapsed().as_micros()).unwrap_or(u32::MAX);
            // Clients may stop ACKing while the window is non-empty. IronRDP
            // keeps the EGFX generation and frame IDs, so clear the structural
            // bookkeeping for the fixed-size timing table. Outstanding frame
            // accounting remains in IronRDP, which treats the ACK suspension as
            // a distinct non-backpressured state per MS-RDPEGFX.
            if queue_depth == u32::MAX {
                times.clear();
            }
            // EWMA with alpha 1/8: smooth enough to ignore single hiccups, quick enough to react.
            let old = self.ack_latency_us.load(Ordering::Acquire);
            let new = if old == 0 {
                sample
            } else {
                old - old / 8 + sample / 8
            };
            self.ack_latency_us.store(new, Ordering::Release);
            if acked <= 3 || acked.is_multiple_of(30) {
                info!(
                    frame_id,
                    acked,
                    queue_depth,
                    total_frames_decoded,
                    latency_us = sample,
                    "EGFX frame acknowledged"
                );
            }
        }
    }

    pub(crate) fn on_qoe_metrics(&self, decode_render_ms: u16) {
        self.client_decode_render_ms
            .store(u32::from(decode_render_ms), Ordering::Release);
        self.client_qoe_reports.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn client_feedback(&self) -> ClientFeedback {
        let ack_received = self.client_ack_received.load(Ordering::Acquire);
        let decode_render = self.client_decode_render_ms.load(Ordering::Acquire);
        ClientFeedback {
            queue_depth: ack_received
                .then(|| self.client_queue_depth.load(Ordering::Acquire))
                .filter(|depth| *depth != 0),
            ack_suspended: self.client_ack_suspended.load(Ordering::Acquire),
            frames_decoded: ack_received
                .then(|| self.client_frames_decoded.load(Ordering::Acquire)),
            decode_render_ms: (self.client_qoe_reports.load(Ordering::Acquire) > 0)
                .then(|| u16::try_from(decode_render).unwrap_or(u16::MAX)),
            qoe_reports: self.client_qoe_reports.load(Ordering::Acquire),
        }
    }

    pub(crate) fn observe_autodetect(
        &self,
        handles: ironrdp_server::autodetect::AutoDetectHandles,
    ) {
        *self.network.lock().unwrap_or_else(|e| e.into_inner()) = Some(handles);
    }

    pub(crate) fn network_diagnostics(&self) -> NetworkDiagnostics {
        let value = |value: u32| (value != u32::MAX).then_some(value);
        let network = self.network.lock().unwrap_or_else(|e| e.into_inner());
        let Some(handles) = network.as_ref() else {
            return NetworkDiagnostics {
                rtt_ms: None,
                baseline_rtt_ms: None,
                bandwidth_kbps: None,
            };
        };
        NetworkDiagnostics {
            rtt_ms: value(handles.rtt.load(Ordering::Acquire)),
            baseline_rtt_ms: value(handles.baseline_rtt.load(Ordering::Acquire)),
            bandwidth_kbps: value(handles.bandwidth.load(Ordering::Acquire)),
        }
    }

    pub(crate) fn observe_client_core(&self, core: &ClientCoreData) {
        *self.client_core.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(ClientCoreSummary::from_core(core));
    }

    pub(crate) fn client_core(&self) -> Option<ClientCoreSummary> {
        self.client_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Records the negotiated EGFX facts for the host diagnostics overlay.
    pub(crate) fn set_egfx_diagnostics(&self, diagnostics: EgfxDiagnostics) {
        let path = match diagnostics.selected_path.as_deref() {
            Some("AVC420") => PATH_AVC420,
            Some("RemoteFXProgressive") => PATH_PROGRESSIVE,
            Some("LegacyBitmap") => PATH_BITMAP,
            _ => PATH_NONE,
        };
        self.egfx_path.store(path, Ordering::Release);
        self.egfx_capability_version.store(
            diagnostics.capability_version.unwrap_or(0),
            Ordering::Release,
        );
        self.egfx_avc420_capability
            .store(diagnostics.avc420, Ordering::Release);
        self.egfx_avc444_capability
            .store(diagnostics.avc444, Ordering::Release);
        self.egfx_progressive_capability
            .store(diagnostics.progressive, Ordering::Release);
        self.egfx_bitmap_capability
            .store(diagnostics.bitmap, Ordering::Release);
        *self
            .egfx_capability_versions
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = diagnostics.capability_versions;
    }

    /// Records classic RDP bitmap codecs advertised by the client.
    pub(crate) fn set_legacy_codec_capabilities(&self, capabilities: LegacyCodecCapabilities) {
        *self.legacy_codecs.lock().unwrap_or_else(|e| e.into_inner()) = capabilities;
    }

    pub(crate) fn egfx_diagnostics(&self) -> EgfxDiagnostics {
        EgfxDiagnostics {
            capability_versions: self
                .egfx_capability_versions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            capability_version: match self.egfx_capability_version.load(Ordering::Acquire) {
                0 => None,
                value => Some(value),
            },
            avc420: self.egfx_avc420_capability.load(Ordering::Acquire),
            avc444: self.egfx_avc444_capability.load(Ordering::Acquire),
            progressive: self.egfx_progressive_capability.load(Ordering::Acquire),
            bitmap: self.egfx_bitmap_capability.load(Ordering::Acquire),
            selected_path: match self.egfx_path.load(Ordering::Acquire) {
                PATH_AVC420 => Some("AVC420".to_owned()),
                PATH_PROGRESSIVE => Some("RemoteFXProgressive".to_owned()),
                PATH_BITMAP => Some("LegacyBitmap".to_owned()),
                _ => None,
            },
        }
    }

    pub(crate) fn legacy_codecs(&self) -> LegacyCodecCapabilities {
        self.legacy_codecs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn selected_legacy_codec(&self) -> Option<String> {
        let codecs = self.legacy_codecs();
        if codecs.qoi_z {
            Some("QOI-Z".to_owned())
        } else if codecs.qoi {
            Some("QOI".to_owned())
        } else if codecs.remote_fx {
            Some("RemoteFX".to_owned())
        } else if codecs.image_remote_fx {
            Some("Image RemoteFX".to_owned())
        } else if codecs.ns_codec {
            Some("NSCodec".to_owned())
        } else {
            Some("RDP6 bitmap".to_owned())
        }
    }

    pub(crate) fn channel_open(&self) -> bool {
        self.channel_open.load(Ordering::Acquire)
    }

    /// DCHANNEL_NONE (0) means DRDYNVC has not assigned a graphics channel yet.
    pub(crate) fn channel_id(&self) -> Option<u32> {
        let handle = self.lock().handle.clone()?;
        handle
            .try_lock()
            .ok()
            .and_then(|gfx| gfx.channel_id().filter(|id| *id != 0))
    }

    /// Handler callback, or session end: the graphics channel is gone.
    pub(crate) fn on_close(&self) {
        let mut event_state = self.event_state.lock().unwrap_or_else(|e| e.into_inner());
        self.avc420_supported.store(false, Ordering::Release);
        self.progressive_supported.store(false, Ordering::Release);
        self.bitmap_supported.store(false, Ordering::Release);
        self.egfx_path.store(PATH_NONE, Ordering::Release);
        self.egfx_capability_version.store(0, Ordering::Release);
        self.egfx_avc420_capability.store(false, Ordering::Release);
        self.egfx_avc444_capability.store(false, Ordering::Release);
        self.egfx_progressive_capability
            .store(false, Ordering::Release);
        self.egfx_bitmap_capability.store(false, Ordering::Release);
        self.egfx_capability_versions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.client_queue_depth.store(0, Ordering::Release);
        self.client_ack_received.store(false, Ordering::Release);
        self.client_ack_suspended.store(false, Ordering::Release);
        self.client_frames_decoded.store(0, Ordering::Release);
        self.client_decode_render_ms.store(0, Ordering::Release);
        self.client_qoe_reports.store(0, Ordering::Release);
        *self.network.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.legacy_codecs.lock().unwrap_or_else(|e| e.into_inner()) =
            LegacyCodecCapabilities::default();
        self.bitmap_mailbox
            .send_modify(|mailbox| mailbox.frame = None);
        if self.channel_open.swap(false, Ordering::AcqRel) {
            event_state.announced_layout = None;
            info!("EGFX closed; stopping video");
            (self.sink)(CoreEvent::VideoStop);
        }
    }

    /// Called by the accept loop when a connection ends.
    pub fn session_ended(&self) {
        self.suppression_monitor_generation
            .fetch_add(1, Ordering::AcqRel);
        self.display_suppressed.store(false, Ordering::Release);
        self.on_close();
        *self.client_core.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.set_client_scale_percent(None, None);
        let mut inner = self.lock();
        self.session_size.store(pack(1920, 1080), Ordering::Release);
        self.initial_pattern_sent.store(false, Ordering::Release);
        self.initial_frame_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.initial_frame_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.bitmap_mailbox.send_modify(|mailbox| {
            mailbox.size = (1920, 1080);
            mailbox.frame = None;
        });
        inner.handle = None;
        inner.suppression_source = None;
        inner.surface = None;
        inner.need_keyframe = true;
        drop(inner);
        self.send_times
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Cheap check the host can make before spending time encoding.
    pub fn can_accept(&self) -> bool {
        if !self.channel_open.load(Ordering::Acquire)
            || !self.avc420_supported.load(Ordering::Acquire)
        {
            return false;
        }
        let (handle, surface_needs_reset) = {
            let inner = self.lock();
            if self.is_display_suppressed(&inner) {
                return false;
            }
            (inner.handle.clone(), inner.surface.is_none())
        };
        let Some(handle) = handle else { return false };
        // Never block the capture thread on the pipeline; if it's busy, say "not now".
        match handle.try_lock() {
            // A failed output enqueue leaves a local frame ID in IronRDP's tracker. The next
            // submission resets graphics and clears that tracker before attempting a keyframe.
            Ok(gfx) => {
                // Capabilities may be negotiated before DRDYNVC assigns the EGFX
                // channel. Do not create a surface or encode a frame until both
                // state machines are ready; otherwise drain_output() has nowhere
                // to send the ResetGraphics/CreateSurface/frame PDUs.
                gfx.is_ready()
                    && gfx.channel_id().is_some()
                    && (surface_needs_reset || !gfx.should_backpressure())
            }
            Err(_) => false,
        }
    }

    /// Cheap gate for captured BGRA frames used when AVC420 was explicitly disabled.
    pub fn can_accept_bitmap(&self) -> bool {
        if !self.channel_open.load(Ordering::Acquire)
            || !self.bitmap_supported.load(Ordering::Acquire)
        {
            return false;
        }
        let inner = self.lock();
        !self.is_display_suppressed(&inner)
    }

    /// Replaces the per-session bitmap mailbox with the latest tightly packed BGRA frame.
    /// A stride is accepted so Swift can submit ScreenCaptureKit's IOSurface without a staging copy.
    pub fn submit_bitmap(&self, data: &[u8], width: u16, height: u16, stride: usize) -> bool {
        if !self.can_accept_bitmap() || (width, height) != self.session_size() {
            return false;
        }
        let size = ironrdp_server::DesktopSize { width, height };
        if !crate::display::is_supported_desktop_size(size) {
            return false;
        }
        let row_bytes = usize::from(width) * 4;
        let Some(required) = stride.checked_mul(usize::from(height)) else {
            return false;
        };
        if stride < row_bytes || data.len() < required {
            return false;
        }

        let Some(packed_len) = row_bytes.checked_mul(usize::from(height)) else {
            return false;
        };
        let copy_started = Instant::now();
        // ScreenCaptureKit commonly pads each IOSurface row. Contiguous input can use
        // one bulk copy; padded input copies directly into the final allocation so the
        // hot path does not repeatedly grow/check a Vec.
        let packed = if stride == row_bytes {
            Bytes::copy_from_slice(&data[..packed_len])
        } else {
            let mut packed = Vec::with_capacity(packed_len);
            for row in data.chunks(stride).take(usize::from(height)) {
                packed.extend_from_slice(&row[..row_bytes]);
            }
            Bytes::from(packed)
        };
        let sequence = self.bitmap_sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let copy_us = copy_started.elapsed().as_micros() as u64;
        debug!(
            sequence,
            width,
            height,
            stride,
            bytes = packed_len,
            copy_us,
            "Bitmap frame copied into mailbox"
        );
        let frame = BitmapFrame {
            sequence,
            data: packed,
            queued_at: Instant::now(),
        };
        let mut accepted = false;
        self.bitmap_mailbox.send_modify(|mailbox| {
            if mailbox.size == (width, height) {
                mailbox.frame = Some(frame);
                accepted = true;
            }
        });
        accepted
    }

    pub fn stats(&self) -> VideoStats {
        let inner = self.lock();
        let times = self.send_times.lock().unwrap_or_else(|e| e.into_inner());
        let acked = self.frames_acked.load(Ordering::Acquire);
        let frames_in_flight = times.len();
        VideoStats {
            frames_sent: inner.frames_sent,
            bytes_sent: inner.bytes_sent,
            frames_dropped: inner.frames_dropped,
            frames_acked: acked,
            bytes_acked: self.bytes_acked.load(Ordering::Acquire),
            frames_in_flight: u32::try_from(frames_in_flight).unwrap_or(u32::MAX),
            ack_latency_us: self.ack_latency_us.load(Ordering::Acquire),
        }
    }

    /// Encodes and queues one RemoteFX Progressive frame on the EGFX channel.
    ///
    /// The host still owns capture and scheduling. Encoding stays in the Rust protocol core
    /// because `WireToSurface2` and the progressive block format are MS-RDPEGFX details.
    pub fn submit_progressive(
        &self,
        data: &[u8],
        width: u16,
        height: u16,
        stride: usize,
    ) -> SubmitResult {
        if !self.can_accept_progressive() || (width, height) != self.session_size() {
            return SubmitResult::NotReady;
        }
        let mut inner = self.lock();
        if self.is_display_suppressed(&inner) {
            return SubmitResult::NotReady;
        }
        let (Some(handle), Some(sender)) = (inner.handle.clone(), inner.sender.clone()) else {
            return SubmitResult::NotReady;
        };

        let encode_started = Instant::now();
        let encoded = match encode_bgra_frame(data, width, height, stride) {
            Ok(encoded) => encoded,
            Err(error) => {
                warn!(
                    session = inner.session_id,
                    width,
                    height,
                    ?error,
                    "RemoteFX Progressive frame encoding failed"
                );
                inner.frames_dropped += 1;
                return SubmitResult::NeedKeyframe;
            }
        };
        let encode_us = u32::try_from(encode_started.elapsed().as_micros()).unwrap_or(u32::MAX);
        let encoded_bytes = encoded.len();
        let timestamp_ms = u32::try_from(inner.started.elapsed().as_millis()).unwrap_or(u32::MAX);

        let (frame_id, channel_id, messages, supports_progressive, backpressure) = {
            let mut gfx = handle.lock().unwrap_or_else(|e| e.into_inner());
            if !gfx.is_ready() {
                return SubmitResult::NotReady;
            }
            let Some(channel_id) = gfx.channel_id() else {
                return SubmitResult::NotReady;
            };
            let backpressure = gfx.should_backpressure();
            let surface = match inner.surface {
                Some(id) => id,
                None => {
                    let monitor = Monitor {
                        left: 0,
                        top: 0,
                        right: i32::from(width) - 1,
                        bottom: i32::from(height) - 1,
                        flags: MonitorFlags::PRIMARY,
                    };
                    gfx.resize_with_monitors(width, height, vec![monitor]);
                    let Some(id) = gfx.create_surface(width, height) else {
                        warn!("RemoteFX Progressive surface creation failed");
                        return SubmitResult::NotReady;
                    };
                    gfx.map_surface_to_output(id, 0, 0);
                    inner.surface = Some(id);
                    info!(
                        session = inner.session_id,
                        surface = id,
                        width,
                        height,
                        "RemoteFX Progressive ResetGraphics/CreateSurface/MapSurface queued"
                    );
                    id
                }
            };
            let frame_id = gfx.send_remotefx_progressive_frame(surface, 0, encoded, timestamp_ms);
            (
                frame_id,
                Some(channel_id),
                gfx.drain_output(),
                self.progressive_supported.load(Ordering::Acquire),
                backpressure,
            )
        };

        let log_frame =
            inner.frames_sent < 3 || inner.frames_sent.saturating_add(1).is_multiple_of(30);
        let had_messages = !messages.is_empty();
        let mut output_queued = false;
        if let (Some(channel_id), false) = (channel_id, messages.is_empty()) {
            match ironrdp_dvc::encode_dvc_messages(
                channel_id,
                messages,
                ChannelFlags::SHOW_PROTOCOL,
            ) {
                Ok(messages) => {
                    if let Some(frame_id) = frame_id {
                        self.send_times
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(frame_id, Instant::now(), encoded_bytes as u64);
                    }
                    if let Err(error) =
                        sender.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                            messages,
                        }))
                    {
                        if let Some(frame_id) = frame_id {
                            self.send_times
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(frame_id);
                        }
                        warn!(
                            session = inner.session_id,
                            error = %error,
                            "RemoteFX Progressive output queue closed"
                        );
                    } else {
                        output_queued = true;
                    }
                }
                Err(error) => warn!(
                    session = inner.session_id,
                    error = %error,
                    "Failed to encode RemoteFX Progressive EGFX messages"
                ),
            }
        }

        if frame_id.is_some() && output_queued {
            inner.frames_sent += 1;
            inner.bytes_sent += encoded_bytes as u64;
            if log_frame {
                info!(
                    session = inner.session_id,
                    frame_id = frame_id.unwrap_or_default(),
                    frame_number = inner.frames_sent,
                    width,
                    height,
                    encoded_bytes,
                    encode_us,
                    "RemoteFX Progressive frame queued"
                );
            }
            SubmitResult::Sent
        } else {
            warn!(
                session = inner.session_id,
                width,
                height,
                supports_progressive,
                backpressure,
                "RemoteFX Progressive frame rejected"
            );
            inner.frames_dropped += 1;
            if frame_id.is_some() || had_messages {
                inner.surface = None;
            }
            SubmitResult::NeedKeyframe
        }
    }

    /// Cheap gate for captured BGRA frames when AVC420 is disabled and Progressive is selected.
    pub fn can_accept_progressive(&self) -> bool {
        if !self.channel_open.load(Ordering::Acquire)
            || !self.progressive_supported.load(Ordering::Acquire)
        {
            return false;
        }
        let (handle, surface_needs_reset) = {
            let inner = self.lock();
            if self.is_display_suppressed(&inner) {
                return false;
            }
            (inner.handle.clone(), inner.surface.is_none())
        };
        let Some(handle) = handle else { return false };
        match handle.try_lock() {
            Ok(gfx) => {
                gfx.is_ready()
                    && gfx.channel_id().is_some()
                    && (surface_needs_reset || !gfx.should_backpressure())
            }
            Err(_) => false,
        }
    }

    /// Lets the host force the next newly captured sample to be an IDR after client output resumes.
    pub fn needs_keyframe(&self) -> bool {
        self.lock().need_keyframe
    }

    /// Queues one H.264 access unit (Annex-B, with SPS/PPS on keyframes) encoded for a
    /// `width`×`height` desktop. Frames for any other size (in flight across a resize) are
    /// refused with `NotReady`, since they would not fit the client's surface.
    pub fn submit_h264(
        &self,
        data: &[u8],
        keyframe: bool,
        width: u16,
        height: u16,
    ) -> SubmitResult {
        if data.is_empty()
            || !self.channel_open.load(Ordering::Acquire)
            || !self.avc420_supported.load(Ordering::Acquire)
        {
            debug!(
                keyframe,
                width,
                height,
                avc420_supported = self.avc420_supported.load(Ordering::Acquire),
                "Encoded video rejected: EGFX AVC420 is not ready"
            );
            return SubmitResult::NotReady;
        }
        let mut inner = self.lock();
        let (w, h) = self.session_size();
        if (width, height) != (w, h) {
            debug!(width, height, session_size = ?(w, h), "Dropping frame for a stale size");
            return SubmitResult::NotReady;
        }
        if self.is_display_suppressed(&inner) {
            return SubmitResult::NotReady;
        }
        let (Some(handle), Some(sender)) = (inner.handle.clone(), inner.sender.clone()) else {
            return SubmitResult::NotReady;
        };
        if inner.need_keyframe && !keyframe {
            inner.frames_dropped += 1;
            debug!(
                session = inner.session_id,
                "Dropping P-frame while waiting for a keyframe"
            );
            return SubmitResult::NeedKeyframe;
        }

        let timestamp_ms = u32::try_from(inner.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let (frame_id, channel_id, messages, supports_avc420, backpressure) = {
            let mut gfx = handle.lock().unwrap_or_else(|e| e.into_inner());
            if !gfx.is_ready() {
                debug!(
                    session = inner.session_id,
                    reason = "egfx_not_ready",
                    "Encoded video deferred"
                );
                return SubmitResult::NotReady;
            }
            let Some(channel_id) = gfx.channel_id() else {
                debug!(
                    session = inner.session_id,
                    reason = "dvc_channel_missing",
                    "Encoded video deferred until EGFX dynamic channel is assigned"
                );
                return SubmitResult::NotReady;
            };
            let supports_avc420 = gfx.supports_avc420();
            let backpressure = gfx.should_backpressure();
            let surface = match inner.surface {
                Some(id) => id,
                None => {
                    // ResetGraphics with one primary monitor, then a surface mapped at (0,0).
                    let monitor = Monitor {
                        left: 0,
                        top: 0,
                        right: i32::from(w) - 1,
                        bottom: i32::from(h) - 1,
                        flags: MonitorFlags::PRIMARY,
                    };
                    gfx.resize_with_monitors(w, h, vec![monitor]);
                    let Some(id) = gfx.create_surface(w, h) else {
                        warn!("EGFX surface creation failed");
                        return SubmitResult::NotReady;
                    };
                    gfx.map_surface_to_output(id, 0, 0);
                    info!(
                        session = inner.session_id,
                        surface = id,
                        width = w,
                        height = h,
                        "EGFX ResetGraphics/CreateSurface/MapSurface queued"
                    );
                    inner.surface = Some(id);
                    id
                }
            };
            let region = Avc420Region {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
                quantization_parameter: 22,
                quality: 100,
            };
            let frame_id = gfx.send_avc420_frame(surface, data, &[region], timestamp_ms);
            (
                frame_id,
                Some(channel_id),
                gfx.drain_output(),
                supports_avc420,
                backpressure,
            )
        };
        let mut output_queued = false;
        let had_messages = !messages.is_empty();
        let log_frame = inner.frames_sent < 3
            || inner.frames_sent.saturating_add(1).is_multiple_of(30)
            || keyframe;

        if !messages.is_empty() {
            if let Some(channel_id) = channel_id {
                let message_count = messages.len();
                let pdu_names: Vec<_> = messages.iter().map(|message| message.name()).collect();
                let pdu_bytes: usize = messages.iter().map(|message| message.size()).sum();
                match ironrdp_dvc::encode_dvc_messages(
                    channel_id,
                    messages,
                    ChannelFlags::SHOW_PROTOCOL,
                ) {
                    Ok(messages) => {
                        if log_frame {
                            info!(
                                session = inner.session_id,
                                frame_id = ?frame_id,
                                channel_id,
                                message_count,
                                pdu_names = ?pdu_names,
                                pdu_bytes,
                                keyframe,
                                payload_bytes = data.len(),
                                "EGFX output encoded for session transport"
                            );
                        }
                        if let Some(frame_id) = frame_id {
                            self.send_times
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(frame_id, Instant::now(), data.len() as u64);
                        }
                        if let Err(error) =
                            sender.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                                messages,
                            }))
                        {
                            if let Some(frame_id) = frame_id {
                                self.send_times
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .remove(frame_id);
                            }
                            warn!(session = inner.session_id, error = %error, "EGFX output queue closed");
                        } else {
                            output_queued = true;
                            if log_frame {
                                info!(session = inner.session_id, frame_id = ?frame_id, "EGFX output queued to session loop");
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "Failed to encode EGFX messages"),
                }
            } else {
                warn!(session = inner.session_id, frame_id = ?frame_id, "EGFX output has no dynamic channel ID");
            }
        }

        if frame_id.is_some() && output_queued {
            inner.need_keyframe = false;
            inner.frames_sent += 1;
            inner.bytes_sent += data.len() as u64;
            if log_frame {
                info!(
                    session = inner.session_id,
                    frame_id = frame_id.unwrap_or_default(),
                    frame_number = inner.frames_sent,
                    keyframe,
                    payload_bytes = data.len(),
                    nal_types = ?h264_nal_types(data),
                    "EGFX video frame queued"
                );
            }
            SubmitResult::Sent
        } else {
            warn!(
                session = inner.session_id,
                keyframe,
                payload_bytes = data.len(),
                supports_avc420,
                backpressure,
                "EGFX rejected encoded video frame"
            );
            inner.need_keyframe = true;
            inner.frames_dropped += 1;
            // The pipeline already allocated this frame ID, and may have queued surface setup
            // messages that were not delivered. Force the next accepted attempt through
            // ResetGraphics, which clears the stale frame tracker before creating a new surface.
            if frame_id.is_some() || had_messages {
                inner.surface = None;
            }
            SubmitResult::NeedKeyframe
        }
    }

    fn is_display_suppressed(&self, inner: &Inner) -> bool {
        self.display_suppressed.load(Ordering::Acquire)
            || inner
                .suppression_source
                .as_ref()
                .is_some_and(|source| source.load(Ordering::Acquire))
    }
}

fn valid_client_scale_percent(
    desktop_scale: Option<u32>,
    device_scale: Option<u32>,
) -> Option<u32> {
    let desktop_scale = desktop_scale.filter(|value| (100..=500).contains(value))?;
    match device_scale? {
        100 | 140 | 180 => Some(desktop_scale),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppressed_output_blocks_video_even_before_surface_creation() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        hub.display_suppressed.store(true, Ordering::Release);
        assert!(!hub.can_accept());
        assert_eq!(
            hub.submit_h264(&[0, 0, 0, 1], true, 1920, 1080),
            SubmitResult::NotReady
        );
        assert!(hub.lock().surface.is_none());
    }

    #[tokio::test]
    async fn suppression_observer_starts_with_source_state() {
        let hub = Arc::new(VideoHub::new(std::sync::Arc::new(|_| {})));
        hub.observe_display_suppression(Arc::new(AtomicBool::new(true)));
        assert!(!hub.can_accept());
    }

    #[test]
    fn suppression_gate_reads_source_before_polling_observer() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        let mut inner = hub.lock();
        inner.suppression_source = Some(Arc::new(AtomicBool::new(true)));

        assert!(hub.is_display_suppressed(&inner));
    }

    #[test]
    fn avc420_disabled_selects_bitmap_fallback_without_accepting_h264() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hub = VideoHub::new(std::sync::Arc::new(move |event| {
            tx.send(event).expect("event receiver remains open");
        }));

        hub.on_ready(false, false);

        assert_eq!(
            rx.try_recv().expect("bitmap fallback starts capture"),
            CoreEvent::BitmapStart {
                width: 1920,
                height: 1080,
                scale_percent: None,
            }
        );
        assert!(!hub.can_accept());
        hub.request_resize(1600, 900, Some(100), Some(100));
        assert_eq!(
            rx.try_recv()
                .expect("bitmap fallback follows client resize"),
            CoreEvent::BitmapResize {
                width: 1600,
                height: 900,
                scale_percent: Some(100),
            }
        );
        assert_eq!(
            hub.submit_h264(&[0, 0, 0, 1], true, 1920, 1080),
            SubmitResult::NotReady
        );
    }

    #[test]
    fn avc420_negotiation_requests_host_video_capture() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hub = VideoHub::new(std::sync::Arc::new(move |event| {
            tx.send(event).expect("event receiver remains open");
        }));

        hub.on_ready(true, false);

        assert_eq!(
            rx.try_recv().expect("AVC420 starts video"),
            CoreEvent::VideoStart {
                width: 1920,
                height: 1080,
                scale_percent: None,
            }
        );
    }

    #[test]
    fn bitmap_fallback_keeps_only_latest_valid_frame() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        let mut mailbox = hub.subscribe_bitmap();
        hub.set_session_size(200, 200);
        hub.on_ready(false, false);

        let stride = 808;
        let mut first = vec![0; stride * 200];
        first[0..4].copy_from_slice(&[1, 2, 3, 4]);
        let mut latest = vec![0; stride * 200];
        latest[0..4].copy_from_slice(&[5, 6, 7, 8]);
        latest[stride..stride + 4].copy_from_slice(&[9, 10, 11, 12]);

        assert!(hub.submit_bitmap(&first, 200, 200, stride));
        assert!(!hub.submit_bitmap(&latest, 201, 200, stride));
        assert!(hub.submit_bitmap(&latest, 200, 200, stride));

        let state = mailbox.borrow_and_update().clone();
        let frame = state.frame.expect("latest bitmap is retained");
        assert_eq!(state.size, (200, 200));
        assert_eq!(frame.data.len(), 200 * 200 * 4);
        assert_eq!(&frame.data[..4], &[5, 6, 7, 8]);
        assert_eq!(&frame.data[200 * 4..200 * 4 + 4], &[9, 10, 11, 12]);
        assert_eq!(frame.sequence, 2);
    }

    #[test]
    fn bitmap_fallback_copies_contiguous_stride_without_padding() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        let mut mailbox = hub.subscribe_bitmap();
        hub.set_session_size(200, 200);
        hub.on_ready(false, false);

        let stride = 800;
        let mut frame = vec![0; stride * 200];
        frame[0..4].copy_from_slice(&[1, 2, 3, 4]);
        frame[199 * stride..199 * stride + 4].copy_from_slice(&[5, 6, 7, 8]);

        assert!(hub.submit_bitmap(&frame, 200, 200, stride));
        let state = mailbox.borrow_and_update().clone();
        let frame = state.frame.expect("contiguous bitmap is retained");
        assert_eq!(frame.data.len(), 200 * 200 * 4);
        assert_eq!(&frame.data[..4], &[1, 2, 3, 4]);
        assert_eq!(&frame.data[199 * 200 * 4..199 * 200 * 4 + 4], &[5, 6, 7, 8]);
    }

    #[test]
    fn avc420_session_rejects_legacy_bitmap_frames() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        hub.set_session_size(200, 200);
        hub.on_ready(true, false);

        assert!(!hub.can_accept_bitmap());
        assert!(!hub.submit_bitmap(&vec![0; 200 * 200 * 4], 200, 200, 800));
    }

    #[test]
    fn initial_client_scale_is_reported_with_video_start() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hub = VideoHub::new(std::sync::Arc::new(move |event| {
            tx.send(event).expect("event receiver remains open");
        }));
        hub.set_client_scale_percent(Some(200), Some(180));

        hub.on_ready(true, false);

        assert_eq!(
            rx.try_recv().expect("AVC420 starts video"),
            CoreEvent::VideoStart {
                width: 1920,
                height: 1080,
                scale_percent: Some(200),
            }
        );
    }

    #[test]
    fn scale_only_layout_change_emits_resize_without_changing_pixel_size() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hub = VideoHub::new(std::sync::Arc::new(move |event| {
            tx.send(event).expect("event receiver remains open");
        }));
        hub.set_client_scale_percent(Some(100), Some(100));
        hub.on_ready(true, false);
        let _ = rx.try_recv().expect("initial video event");

        hub.request_resize(1920, 1080, Some(200), Some(180));

        assert_eq!(hub.session_size(), (1920, 1080));
        assert_eq!(
            rx.try_recv()
                .expect("scale-only changes restart display setup"),
            CoreEvent::VideoResize {
                width: 1920,
                height: 1080,
                scale_percent: Some(200),
            }
        );
        hub.request_resize(1920, 1080, Some(200), Some(180));
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn invalid_initial_client_scale_is_unknown() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        hub.set_client_scale_percent(Some(501), Some(180));

        assert_eq!(hub.client_scale_percent(), None);
    }

    #[test]
    fn ending_session_resets_client_layout_state() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        hub.set_session_size(1440, 2560);
        hub.set_client_scale_percent(Some(200), Some(180));

        hub.session_ended();

        assert_eq!(hub.session_size(), (1920, 1080));
        assert_eq!(hub.client_scale_percent(), None);
    }

    #[test]
    fn resize_invalidates_surface_and_requires_a_keyframe() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        hub.lock().surface = Some(7);
        hub.lock().need_keyframe = false;
        hub.request_resize(1600, 900, None, None);

        assert_eq!(hub.session_size(), (1600, 900));
        let inner = hub.lock();
        assert_eq!(inner.surface, None);
        assert!(inner.need_keyframe);
    }

    #[test]
    fn client_scale_requires_valid_desktop_and_device_pair() {
        assert_eq!(valid_client_scale_percent(Some(200), Some(180)), Some(200));
        assert_eq!(valid_client_scale_percent(Some(200), None), None);
        assert_eq!(valid_client_scale_percent(Some(200), Some(120)), None);
        assert_eq!(valid_client_scale_percent(Some(501), Some(180)), None);
    }

    #[test]
    fn output_resume_emits_refresh_only_for_an_active_surface() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hub = VideoHub::new(std::sync::Arc::new(move |event| {
            tx.send(event).expect("event receiver remains open");
        }));
        hub.channel_open.store(true, Ordering::Release);
        hub.lock().surface = Some(1);

        hub.update_display_suppression(true);
        hub.update_display_suppression(false);

        assert!(hub.needs_keyframe());
        assert_eq!(rx.try_recv(), Ok(CoreEvent::VideoRefresh));
        hub.update_display_suppression(false);
        assert!(rx.try_recv().is_err(), "duplicate resume state is ignored");
    }

    #[test]
    fn bitmap_output_resume_requests_one_fresh_frame_without_an_egfx_surface() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hub = VideoHub::new(std::sync::Arc::new(move |event| {
            tx.send(event).expect("event receiver remains open");
        }));
        hub.channel_open.store(true, Ordering::Release);
        hub.bitmap_supported.store(true, Ordering::Release);

        hub.update_display_suppression(true);
        hub.update_display_suppression(false);

        assert_eq!(rx.try_recv(), Ok(CoreEvent::VideoRefresh));
        hub.update_display_suppression(false);
        assert!(rx.try_recv().is_err(), "duplicate resume state is ignored");
    }

    #[test]
    fn missing_resize_scale_retains_last_valid_client_hint() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hub = VideoHub::new(std::sync::Arc::new(move |event| {
            tx.send(event).expect("event receiver remains open");
        }));
        hub.set_client_scale_percent(Some(200), Some(180));
        hub.on_ready(true, false);
        let _ = rx.try_recv().expect("initial video event");

        hub.request_resize(1600, 900, None, None);

        assert_eq!(hub.client_scale_percent(), Some(200));
        assert_eq!(
            rx.try_recv()
                .expect("resize preserves valid connection hint"),
            CoreEvent::VideoResize {
                width: 1600,
                height: 900,
                scale_percent: Some(200),
            }
        );
    }

    #[test]
    fn acknowledgments_count_only_once_for_queued_frame_ids() {
        let hub = VideoHub::new(std::sync::Arc::new(|_| {}));
        hub.send_times
            .lock()
            .unwrap()
            .insert(42, Instant::now(), 1234);

        hub.on_frame_ack(99, 0, 1);
        assert_eq!(hub.stats().frames_acked, 0, "unknown frame IDs are ignored");
        assert_eq!(hub.stats().frames_in_flight, 1);

        hub.on_frame_ack(42, 0, 1);
        hub.on_frame_ack(42, 0, 2);
        let stats = hub.stats();
        assert_eq!(stats.frames_acked, 1, "duplicate ACKs count once");
        assert_eq!(stats.frames_in_flight, 0);
    }

    #[test]
    fn ack_suspend_clears_clawtank_samples_but_counts_current_ack() {
        let hub = VideoHub::new(Arc::new(|_| {}));
        let mut times = hub.send_times.lock().unwrap();
        times.insert(1, Instant::now(), 100);
        times.insert(2, Instant::now(), 200);
        drop(times);

        hub.on_frame_ack(1, u32::MAX, 1);

        assert_eq!(hub.frames_acked.load(Ordering::Acquire), 1);
        assert!(hub.send_times.lock().unwrap().by_frame.is_empty());
        assert_eq!(hub.stats().frames_in_flight, 0);
    }

    #[test]
    fn pending_ack_timing_samples_remain_bounded_when_acknowledgments_pause() {
        let mut pending = PendingAckTimes::default();
        for frame_id in 0..(MAX_PENDING_ACK_TIMES as u32 + 17) {
            pending.insert(frame_id, Instant::now(), 64);
        }

        assert_eq!(pending.len(), MAX_PENDING_ACK_TIMES);
        assert_eq!(pending.remove(0), None, "oldest sample was evicted");
        assert!(pending.remove(MAX_PENDING_ACK_TIMES as u32 + 16).is_some());
    }

    #[test]
    fn ready_and_resize_events_use_the_latest_committed_layout() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hub = VideoHub::new(std::sync::Arc::new(move |event| {
            tx.send(event).expect("event receiver remains open");
        }));

        hub.request_resize(1600, 900, None, None);
        hub.on_ready(true, false);

        assert_eq!(
            rx.try_recv().expect("ready reports committed layout"),
            CoreEvent::VideoStart {
                width: 1600,
                height: 900,
                scale_percent: None,
            }
        );
        assert!(rx.try_recv().is_err(), "stale resize event is not emitted");
    }
}
