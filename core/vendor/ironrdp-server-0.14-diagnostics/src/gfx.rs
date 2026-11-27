//! EGFX (Graphics Pipeline Extension) server integration.
//!
//! Provides the bridge between `ironrdp-egfx`'s `GraphicsPipelineServer` and
//! `ironrdp-server`'s `RdpServer`, enabling H.264 video streaming via DVC.
//!
//! The bridge pattern (`GfxDvcBridge`) wraps an `Arc<Mutex<GraphicsPipelineServer>>`
//! so the display handler can call `send_avc420_frame()` proactively while the
//! DVC infrastructure handles client messages (capability negotiation, frame acks).

use std::sync::{Arc, Mutex};

use ironrdp_core::impl_as_any;
use ironrdp_dvc::{DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_pdu::PduResult;
use ironrdp_pdu::rdp::capability_sets::EntropyBits;
use ironrdp_svc::SvcMessage;

use crate::autodetect::AutoDetectHandles;
use crate::server::ServerEventSender;

/// Client-side bitmap codec capabilities observed during RDP activation.
///
/// These capabilities are negotiated in the classic RDP capability exchange,
/// before EGFX selects its graphics pipeline. Keeping the observation in a
/// per-connection shared value lets the EGFX handler make a conservative
/// decision without inferring support from a client name or platform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GfxClientCapabilities {
    /// AVC420 was enabled by the final EGFX capability intersection.
    pub avc420: bool,
    /// The peer advertised the legacy RemoteFX bitmap codec.
    pub remote_fx: bool,
    /// The peer advertised Image RemoteFX in the classic BitmapCodecs set.
    /// This is diagnostic metadata only; it does not select EGFX codecs.
    pub image_remote_fx: bool,
    /// The peer advertised NSCodec in the classic BitmapCodecs set.
    pub ns_codec: bool,
    /// The peer advertised QOI in the classic BitmapCodecs set.
    pub qoi: bool,
    /// The peer advertised QOI-Z in the classic BitmapCodecs set.
    pub qoi_z: bool,
    /// RemoteFX entropy coders present in the classic bitmap codec offer.
    pub entropy_bits: Vec<EntropyBits>,
    /// Raw `TS_SURFCMDS_CAPS_SET` command flags from the peer.
    pub surface_command_flags: u32,
}

/// Shared per-connection client capability state.
pub type GfxClientCapabilitiesHandle = Arc<Mutex<GfxClientCapabilities>>;

/// Shared handle to a `GraphicsPipelineServer`.
///
/// Uses `std::sync::Mutex` (not tokio) because `DvcProcessor` trait methods
/// are synchronous and cannot hold async locks.
pub type GfxServerHandle = Arc<Mutex<GraphicsPipelineServer>>;

/// What a connection hands to its EGFX backend, passed to the build methods of
/// [`GfxServerFactory`].
///
/// Each connection has its own handles, the same ones it hands to the display
/// through [`DisplayContext`](crate::DisplayContext).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GfxContext {
    /// The connection's auto-detect measurements, for flow control.
    pub autodetect: AutoDetectHandles,
    /// Client bitmap codec and surface command capabilities observed during
    /// classic capability negotiation.
    pub client_capabilities: GfxClientCapabilitiesHandle,
}

/// Factory for creating EGFX graphics pipeline handlers.
///
/// Implements `ServerEventSender` so the factory can signal the server event loop
/// when EGFX frames are ready to be drained and sent.
pub trait GfxServerFactory: ServerEventSender + Send {
    /// Create a handler for EGFX callbacks (caps negotiation, frame acks).
    fn build_gfx_handler(&self, ctx: GfxContext) -> Box<dyn GraphicsPipelineHandler>;

    /// Create a bridge and shared server handle for proactive frame sending.
    ///
    /// When returning `Some`, the bridge is registered with DrdynvcServer for
    /// client messages, and the handle reaches the display through
    /// [`DisplayContext::gfx_handle`](crate::DisplayContext::gfx_handle) for
    /// direct frame submission. Returns `None` by default, falling back to
    /// `build_gfx_handler()`.
    fn build_server_with_handle(
        &self,
        _ctx: GfxContext,
    ) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        None
    }
}

/// Build an EGFX bridge while always retaining a shared server handle.
pub(crate) fn build_server_and_handle(
    factory: &dyn GfxServerFactory,
    ctx: GfxContext,
) -> (GfxDvcBridge, GfxServerHandle) {
    if let Some((bridge, handle)) = factory.build_server_with_handle(ctx.clone()) {
        return (bridge, handle);
    }

    let handler = factory.build_gfx_handler(ctx);
    let server = Arc::new(Mutex::new(GraphicsPipelineServer::new(handler)));
    (GfxDvcBridge::new(Arc::clone(&server)), server)
}

/// DVC bridge wrapping a shared `GraphicsPipelineServer`.
///
/// Delegates all `DvcProcessor` methods to the inner server through a mutex,
/// enabling shared access from both the DVC layer and the display handler.
pub struct GfxDvcBridge {
    inner: GfxServerHandle,
}

impl GfxDvcBridge {
    pub fn new(server: GfxServerHandle) -> Self {
        Self { inner: server }
    }

    pub fn server(&self) -> &GfxServerHandle {
        &self.inner
    }
}

impl_as_any!(GfxDvcBridge);

impl DvcProcessor for GfxDvcBridge {
    fn channel_name(&self) -> &str {
        ironrdp_egfx::CHANNEL_NAME
    }

    fn max_message_size(&self) -> Option<usize> {
        self.inner
            .lock()
            .expect("GfxServerHandle mutex poisoned")
            .max_message_size()
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.inner
            .lock()
            .expect("GfxServerHandle mutex poisoned")
            .start(channel_id)
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        self.inner
            .lock()
            .expect("GfxServerHandle mutex poisoned")
            .process(channel_id, payload)
    }

    fn close(&mut self, channel_id: u32) {
        self.inner
            .lock()
            .expect("GfxServerHandle mutex poisoned")
            .close(channel_id)
    }
}

impl DvcServerProcessor for GfxDvcBridge {}

/// Message for routing EGFX PDUs to the wire via `ServerEvent`.
#[derive(Debug)]
pub enum EgfxServerMessage {
    /// Pre-encoded DVC messages from `GraphicsPipelineServer::drain_output()`.
    SendMessages { messages: Vec<SvcMessage> },
}

impl core::fmt::Display for EgfxServerMessage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SendMessages { messages } => {
                write!(f, "SendMessages(count={})", messages.len())
            }
        }
    }
}

/// The dynamic channel id EGFX is registered under: the bridge when the
/// factory handed out a frame handle, otherwise the server itself.
pub(crate) fn egfx_channel_id(drdynvc: &ironrdp_dvc::DrdynvcServer) -> Option<u32> {
    drdynvc
        .get_channel_id_by_type::<GfxDvcBridge>()
        .or_else(|| drdynvc.get_channel_id_by_type::<GraphicsPipelineServer>())
}

#[cfg(test)]
mod tests {
    use ironrdp_dvc::DrdynvcServer;
    use ironrdp_egfx::pdu::{CapabilitiesAdvertisePdu, CapabilitySet};

    use super::*;

    struct Handler;

    impl GraphicsPipelineHandler for Handler {
        fn capabilities_advertise(&mut self, _pdu: &CapabilitiesAdvertisePdu) {}

        fn on_ready(&mut self, _negotiated: &CapabilitySet) {}
    }

    struct LegacyFactory;

    impl ServerEventSender for LegacyFactory {
        fn set_sender(
            &mut self,
            _sender: tokio::sync::mpsc::UnboundedSender<crate::server::ServerEvent>,
        ) {
        }
    }

    impl GfxServerFactory for LegacyFactory {
        fn build_gfx_handler(&self, _: GfxContext) -> Box<dyn GraphicsPipelineHandler> {
            Box::new(Handler)
        }
    }

    #[test]
    fn legacy_factory_path_retains_clampable_handle() {
        let (_bridge, handle) = build_server_and_handle(
            &LegacyFactory,
            GfxContext {
                autodetect: AutoDetectHandles::default(),
                client_capabilities: Arc::default(),
            },
        );
        let mut server = handle.lock().expect("GfxServerHandle mutex poisoned");
        server.set_max_frames_in_flight(3);
        assert_eq!(
            server.set_client_frame_ack_limit(Some(
                core::num::NonZeroU32::new(1).expect("one is non-zero")
            )),
            1
        );
    }

    #[test]
    fn egfx_is_found_whichever_way_it_was_registered() {
        let server = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(Handler))));
        let bridged = DrdynvcServer::new().with_dynamic_channel(GfxDvcBridge::new(server));
        assert!(egfx_channel_id(&bridged).is_some());

        let direct = DrdynvcServer::new()
            .with_dynamic_channel(GraphicsPipelineServer::new(Box::new(Handler)));
        assert!(egfx_channel_id(&direct).is_some());

        assert!(egfx_channel_id(&DrdynvcServer::new()).is_none());
    }
}
