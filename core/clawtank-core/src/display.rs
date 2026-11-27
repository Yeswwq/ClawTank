//! Legacy (non-EGFX) display source: one static test frame per connection. With EGFX the
//! live H.264 stream (see `video.rs`) replaces it as soon as the first frame arrives.
//!
//! The frame is sized to the client's requested desktop (clamped) and sent once, so an idle
//! session costs no bandwidth. Live capture replaces this in Phase 2.
//!
//! After the frame, the same stream carries cursor shapes from [`PointerHub`].
//!
//! IronRDP calls [`RdpServerDisplay::updates`] only after the client has authenticated and
//! finished capability exchange, so that call doubles as our "session started" signal.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;

use bytes::Bytes;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayContext, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates, ServerResult,
};
use tracing::{debug, info, warn};

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::watch;

use crate::access::SessionTracker;
use crate::pointer::PointerHub;
use crate::test_pattern;
use crate::video::{BitmapMailbox, VideoHub};

const DEFAULT_SIZE: DesktopSize = DesktopSize {
    width: 1920,
    height: 1080,
};
// Keep the advertised single-monitor pixel-area budget while allowing either orientation.
pub(crate) const MAX_DISPLAY_SIDE: u16 = 3840;
pub(crate) const MAX_DISPLAY_AREA: u64 = 3840 * 2400;
const MIN_SIDE: u16 = 200;

/// Serves the static test pattern.
pub struct TestPatternDisplay {
    tracker: Arc<SessionTracker>,
    video: Arc<VideoHub>,
    pointer: Arc<PointerHub>,
    initial_frame: Option<Vec<u8>>,
    sink: Option<crate::events::EventSink>,
}

impl TestPatternDisplay {
    pub fn new(
        tracker: Arc<SessionTracker>,
        video: Arc<VideoHub>,
        pointer: Arc<PointerHub>,
    ) -> Self {
        Self {
            tracker,
            video,
            pointer,
            initial_frame: None,
            sink: None,
        }
    }

    /// Test-only display that sends a caller-provided tightly packed BGRA frame first.
    pub(crate) fn with_optional_initial_bgra_frame(
        tracker: Arc<SessionTracker>,
        video: Arc<VideoHub>,
        pointer: Arc<PointerHub>,
        initial_frame: Option<Vec<u8>>,
    ) -> Self {
        Self {
            tracker,
            video,
            pointer,
            initial_frame,
            sink: None,
        }
    }

    pub(crate) fn with_sink(mut self, sink: crate::events::EventSink) -> Self {
        self.sink = Some(sink);
        self
    }
}

pub(crate) fn is_supported_desktop_size(size: DesktopSize) -> bool {
    (MIN_SIDE..=MAX_DISPLAY_SIDE).contains(&size.width)
        && (MIN_SIDE..=MAX_DISPLAY_SIDE).contains(&size.height)
        && u64::from(size.width) * u64::from(size.height) <= MAX_DISPLAY_AREA
}

/// AVC420 carries 4:2:0 chroma samples, so both axes must describe an even
/// desktop rectangle. Keep the RDP desktop contract and the VideoToolbox
/// content rectangle identical by rounding a valid request down by one pixel
/// when necessary. The coded H.264 buffer remains 16-aligned in Swift.
pub(crate) fn normalize_video_desktop_size(size: DesktopSize) -> Option<DesktopSize> {
    if !is_supported_desktop_size(size) {
        return None;
    }
    let normalized = DesktopSize {
        width: size.width & !1,
        height: size.height & !1,
    };
    is_supported_desktop_size(normalized).then_some(normalized)
}

#[async_trait::async_trait]
impl RdpServerDisplay for TestPatternDisplay {
    async fn size(&mut self) -> DesktopSize {
        let (width, height) = self.video.session_size();
        DesktopSize { width, height }
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        let size = normalize_video_desktop_size(client_size).unwrap_or_else(|| {
            // Oversized or otherwise invalid negotiated dimensions should be rejected
            // before capability exchange.
            warn!(
                ?client_size,
                "Initial desktop size violates server limits; using default"
            );
            DEFAULT_SIZE
        });
        self.video.set_session_size(size.width, size.height);
        info!(?client_size, chosen = ?size, "Client requested desktop size");
        size
    }

    /// Display Control (MS-RDPEDISP): the client window was resized or moved to another monitor.
    /// We follow the primary monitor; the host resizes its virtual display and restarts video.
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let Some(monitor) = layout
            .monitors()
            .iter()
            .find(|m| m.is_primary())
            .or_else(|| layout.monitors().first())
        else {
            return;
        };
        let (width, height) = monitor.dimensions();
        let requested = u16::try_from(width)
            .ok()
            .zip(u16::try_from(height).ok())
            .map(|(width, height)| DesktopSize { width, height });
        let desktop_scale = monitor.desktop_scale_factor();
        let device_scale = monitor.device_scale_factor().map(|value| value.value());
        let physical_dimensions = monitor.physical_dimensions();
        let Some(size) = requested.and_then(normalize_video_desktop_size) else {
            warn!(
                pixel_width = width,
                pixel_height = height,
                current = ?self.video.session_size(),
                "Ignoring client layout outside server display limits"
            );
            return;
        };
        info!(
            ?size,
            pixel_width = width,
            pixel_height = height,
            normalized = u32::from(size.width) != width || u32::from(size.height) != height,
            position = ?monitor.position(),
            is_primary = monitor.is_primary(),
            monitor_count = layout.monitors().len(),
            physical_dimensions_mm = ?physical_dimensions,
            orientation = ?monitor.orientation(),
            ?desktop_scale,
            ?device_scale,
            "Client requested new layout"
        );
        self.video
            .request_resize(size.width, size.height, desktop_scale, device_scale);
    }

    async fn updates(
        &mut self,
        ctx: DisplayContext,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        self.tracker.session_started();
        self.video
            .observe_display_suppression(ctx.display_suppressed);
        self.video.observe_autodetect(ctx.autodetect);
        let (tx, rx) = unbounded_channel();
        self.pointer.attach(tx);
        let (width, height) = self.video.session_size();
        let bitmap = self.video.subscribe_bitmap();
        self.video.prepare_initial_frame();
        Ok(Box::new(TestPatternUpdates {
            size: DesktopSize { width, height },
            sent: !self.video.take_initial_test_pattern(),
            pointer: rx,
            bitmap,
            last_bitmap_sequence: 0,
            initial_frame: self.initial_frame.take(),
            video: Arc::clone(&self.video),
            sink: self.sink.clone(),
        }))
    }
}

struct TestPatternUpdates {
    size: DesktopSize,
    sent: bool,
    pointer: UnboundedReceiver<DisplayUpdate>,
    bitmap: watch::Receiver<BitmapMailbox>,
    last_bitmap_sequence: u64,
    initial_frame: Option<Vec<u8>>,
    video: Arc<VideoHub>,
    sink: Option<crate::events::EventSink>,
}

impl TestPatternUpdates {
    async fn frame(&self) -> DisplayUpdate {
        let (w, h) = (self.size.width, self.size.height);
        let data = if let Some(data) = self.initial_frame.clone() {
            data
        } else if let Some(data) = self.video.take_initial_frame().await {
            data
        } else {
            warn!(
                width = w,
                height = h,
                "Timed out waiting for host diagnostic bitmap; using built-in test pattern"
            );
            test_pattern::render_bgra(w, h)
        };
        DisplayUpdate::Bitmap(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(w).expect("clamped to >= MIN_SIDE"),
            height: NonZeroU16::new(h).expect("clamped to >= MIN_SIDE"),
            format: PixelFormat::BgrA32,
            data: Bytes::from(data),
            stride: NonZeroUsize::new(usize::from(w) * 4).expect("non-zero width"),
        })
    }

    fn current_bitmap(&mut self) -> Option<DisplayUpdate> {
        let mailbox = self.bitmap.borrow().clone();
        let frame = mailbox.frame?;
        if frame.sequence <= self.last_bitmap_sequence
            || mailbox.size != (self.size.width, self.size.height)
        {
            return None;
        }
        self.last_bitmap_sequence = frame.sequence;
        debug!(
            sequence = frame.sequence,
            width = self.size.width,
            height = self.size.height,
            bytes = frame.data.len(),
            mailbox_age_us = frame.queued_at.elapsed().as_micros() as u64,
            "Legacy bitmap frame dequeued for RDP encoding"
        );
        Some(DisplayUpdate::Bitmap(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(self.size.width).expect("clamped to >= MIN_SIDE"),
            height: NonZeroU16::new(self.size.height).expect("clamped to >= MIN_SIDE"),
            format: PixelFormat::BgrA32,
            data: frame.data,
            stride: NonZeroUsize::new(usize::from(self.size.width) * 4).expect("non-zero width"),
        }))
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for TestPatternUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        if !self.sent {
            if let Some(sink) = &self.sink {
                sink(crate::events::CoreEvent::InitialScreen {
                    width: self.size.width,
                    height: self.size.height,
                });
            }
            self.sent = true;
            info!(
                width = self.size.width,
                height = self.size.height,
                "Sending test frame"
            );
            return Ok(Some(self.frame().await));
        }
        // AVC-disabled sessions use the same serialized display loop with a latest-frame mailbox.
        // The mailbox prevents capture bursts from becoming an unbounded network backlog.
        loop {
            let mailbox = self.bitmap.borrow().clone();
            if mailbox.size != (self.size.width, self.size.height) {
                self.size = DesktopSize {
                    width: mailbox.size.0,
                    height: mailbox.size.1,
                };
                self.last_bitmap_sequence = 0;
                info!(?self.size, "Applying legacy bitmap desktop resize");
                return Ok(Some(DisplayUpdate::Resize(self.size)));
            }
            if let Some(update) = self.current_bitmap() {
                return Ok(Some(update));
            }
            tokio::select! {
                update = self.pointer.recv() => match update {
                    Some(update) => return Ok(Some(update)),
                    None => std::future::pending().await,
                },
                changed = self.bitmap.changed() => {
                    if changed.is_err() {
                        std::future::pending().await
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_sizes_obey_side_and_area_limits() {
        assert!(is_supported_desktop_size(DesktopSize {
            width: 1440,
            height: 2560,
        }));
        assert!(is_supported_desktop_size(DesktopSize {
            width: 3840,
            height: 2400,
        }));
        assert!(!is_supported_desktop_size(DesktopSize {
            width: 3100,
            height: 3000,
        }));
        assert!(!is_supported_desktop_size(DesktopSize {
            width: 4000,
            height: 1000,
        }));
        assert!(!is_supported_desktop_size(DesktopSize {
            width: 9000,
            height: 10,
        }));
        assert!(!is_supported_desktop_size(DesktopSize {
            width: 199,
            height: 768,
        }));
    }

    #[test]
    fn video_sizes_normalize_both_axes_for_avc420() {
        assert_eq!(
            normalize_video_desktop_size(DesktopSize {
                width: 1904,
                height: 1001,
            }),
            Some(DesktopSize {
                width: 1904,
                height: 1000,
            })
        );
        assert_eq!(
            normalize_video_desktop_size(DesktopSize {
                width: 1905,
                height: 1001,
            }),
            Some(DesktopSize {
                width: 1904,
                height: 1000,
            })
        );
        assert_eq!(
            normalize_video_desktop_size(DesktopSize {
                width: 199,
                height: 768,
            }),
            None
        );
    }
}
