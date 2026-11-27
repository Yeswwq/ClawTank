//! IronRDP EGFX glue: the factory that builds a graphics pipeline per connection and the
//! handler that reports pipeline state to the [`VideoHub`].
//!
//! The handler runs with IronRDP's pipeline mutex held, so it only calls `VideoHub`'s
//! atomic-only callbacks (see the lock-order note in `video.rs`).

use std::sync::{Arc, Mutex};

use ironrdp_egfx::pdu::{
    CapabilitiesAdvertisePdu, CapabilitiesV8Flags, CapabilitiesV10Flags, CapabilitiesV81Flags,
    CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitySet,
};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer, QoeMetrics};
use ironrdp_server::{
    GfxClientCapabilitiesHandle, GfxContext, GfxDvcBridge, GfxServerFactory, GfxServerHandle,
    ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;

use crate::video::{EgfxDiagnostics, LegacyCodecCapabilities, VideoHub};

/// Codec path selected from the final EGFX capability set.
///
/// `Avc444` is a possible selected path only when the peer's negotiated
/// capability permits it and the host has a complete dual-view sender. The
/// current host keeps that implementation gate disabled, so a peer that allows
/// AVC444 still receives AVC420 until the sender is wired and verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NegotiatedCodec {
    Avc444,
    Avc420,
    RemoteFxProgressive,
    LegacyBitmap,
}

/// Peer capabilities and the codec path selected by this server implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NegotiatedCodecs {
    pub capability_version: u32,
    pub avc_enabled: bool,
    pub avc444_supported_by_peer: bool,
    pub avc420_supported_by_peer: bool,
    pub progressive_allowed_by_capability: bool,
    pub selected: NegotiatedCodec,
}

impl NegotiatedCodecs {
    /// Select a path using protocol capabilities and host implementation support.
    ///
    /// `avc444_implemented` and `progressive_implemented` are explicit inputs so
    /// a sender cannot silently select a codec merely because the peer supports it.
    /// AVC444 support is implied by RDPGFX 10.x with AVC enabled; it is not a
    /// separate capability bit. This build keeps its sender disabled until the
    /// dual-view encoding path is wired through the FFI boundary.
    fn from_capability(
        capability: &CapabilitySet,
        avc444_implemented: bool,
        progressive_allowed_by_capability: bool,
        progressive_implemented: bool,
    ) -> Self {
        let (avc_enabled, avc420_supported_by_peer, avc444_supported_by_peer) =
            avc_capabilities(capability);
        let selected = if avc444_supported_by_peer && avc444_implemented {
            NegotiatedCodec::Avc444
        } else if avc420_supported_by_peer {
            NegotiatedCodec::Avc420
        } else if progressive_allowed_by_capability && progressive_implemented {
            NegotiatedCodec::RemoteFxProgressive
        } else {
            NegotiatedCodec::LegacyBitmap
        };

        Self {
            capability_version: capability.version().0,
            avc_enabled,
            avc444_supported_by_peer,
            avc420_supported_by_peer,
            progressive_allowed_by_capability,
            selected,
        }
    }
}

pub struct GfxFactory {
    hub: Arc<VideoHub>,
}

impl GfxFactory {
    pub fn new(hub: Arc<VideoHub>) -> Self {
        Self { hub }
    }
}

impl ServerEventSender for GfxFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.hub.set_sender(sender);
    }
}

impl GfxServerFactory for GfxFactory {
    fn build_gfx_handler(&self, ctx: GfxContext) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GfxHandler {
            hub: Arc::clone(&self.hub),
            client_capabilities: ctx.client_capabilities,
            advertised_capability_versions: Vec::new(),
        })
    }

    fn build_server_with_handle(&self, ctx: GfxContext) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server: GfxServerHandle = Arc::new(Mutex::new(GraphicsPipelineServer::new(
            self.build_gfx_handler(ctx),
        )));
        self.hub.attach(Arc::clone(&server));
        Some((GfxDvcBridge::new(Arc::clone(&server)), server))
    }
}

struct GfxHandler {
    hub: Arc<VideoHub>,
    client_capabilities: GfxClientCapabilitiesHandle,
    advertised_capability_versions: Vec<u32>,
}

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        let versions: Vec<u32> = pdu
            .0
            .iter()
            .map(|capability| capability.version.0)
            .collect();
        let parsed: Vec<_> = pdu
            .0
            .iter()
            .filter_map(|capability| capability.parsed().ok().flatten())
            .collect();
        info!(
            capability_versions = ?versions,
            parsed_capabilities = ?parsed,
            "Client EGFX capabilities advertised"
        );
        self.advertised_capability_versions = versions;
    }

    fn preferred_capabilities(&self) -> Vec<CapabilitySet> {
        preferred_capabilities()
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        // Classic BitmapCodecs are negotiated outside EGFX. `ImageRemoteFX`
        // and `SET_SURFACE_BITS` therefore cannot prove that the peer accepts
        // EGFX RemoteFX Progressive (`WireToSurface2`). Derive eligibility only
        // from the final EGFX capability flags. The sender implementation gate
        // remains disabled until its frame path is connected and verified.
        let progressive_allowed_by_capability = allows_remotefx_progressive(negotiated);
        let codecs = NegotiatedCodecs::from_capability(
            negotiated,
            false,
            progressive_allowed_by_capability,
            true,
        );
        let client_capabilities = {
            let mut capabilities = self
                .client_capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            capabilities.avc420 = codecs.avc420_supported_by_peer;
            capabilities.clone()
        };
        // Keep the host route derived from the selected implementation path,
        // rather than from a raw peer flag. AVC444 remains disabled until its
        // dual-view sender exists; Progressive has a complete Rust wire encoder.
        let supports_avc420 = codecs.selected == NegotiatedCodec::Avc420;
        let supports_progressive = codecs.selected == NegotiatedCodec::RemoteFxProgressive;
        let selected_video_path = match codecs.selected {
            NegotiatedCodec::Avc420 => "AVC420",
            NegotiatedCodec::Avc444 => "AVC444",
            NegotiatedCodec::RemoteFxProgressive => "RemoteFXProgressive",
            NegotiatedCodec::LegacyBitmap => "LegacyBitmap",
        };
        let actual_video_path = if supports_avc420 {
            "AVC420"
        } else if supports_progressive {
            "RemoteFXProgressive"
        } else {
            "LegacyBitmap"
        };
        assert_eq!(
            selected_video_path, actual_video_path,
            "negotiated EGFX codec must select the matching host sender"
        );
        let advertised = self
            .client_capabilities
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let legacy = LegacyCodecCapabilities {
            remote_fx: advertised.remote_fx,
            image_remote_fx: advertised.image_remote_fx,
            ns_codec: advertised.ns_codec,
            qoi: advertised.qoi,
            qoi_z: advertised.qoi_z,
            surface_commands: advertised.surface_command_flags,
        };
        drop(advertised);
        self.hub.set_legacy_codec_capabilities(legacy.clone());
        self.hub.set_egfx_diagnostics(EgfxDiagnostics {
            capability_versions: self.advertised_capability_versions.clone(),
            capability_version: Some(codecs.capability_version),
            avc420: codecs.avc420_supported_by_peer,
            avc444: codecs.avc444_supported_by_peer,
            progressive: codecs.progressive_allowed_by_capability,
            bitmap: true,
            selected_path: Some(selected_video_path.to_owned()),
        });
        info!(
            session = self.hub.session_id(),
            capability_version = codecs.capability_version,
            ?negotiated,
            avc_enabled = codecs.avc_enabled,
            supports_avc420,
            avc420_supported_by_peer = codecs.avc420_supported_by_peer,
            avc444_supported_by_peer = codecs.avc444_supported_by_peer,
            progressive_allowed_by_capability = codecs.progressive_allowed_by_capability,
            final_capset_thin_client = egfx_thin_client(negotiated),
            client_remote_fx_bitmap = client_capabilities.remote_fx,
            client_image_remote_fx_bitmap = client_capabilities.image_remote_fx,
            client_surface_command_flags = client_capabilities.surface_command_flags,
            selected_codec = ?codecs.selected,
            selected_video_path,
            actual_video_path,
            host_avc420_sender_selected = supports_avc420,
            host_progressive_sender_selected = supports_progressive,
            avc444_sender_implemented = false,
            progressive_sender_implemented = true,
            "EGFX capabilities confirmed by server"
        );
        self.hub.on_ready(supports_avc420, supports_progressive);
    }

    fn on_frame_ack(&mut self, frame_id: u32, queue_depth: u32, total_frames_decoded: u32) {
        self.hub
            .on_frame_ack(frame_id, queue_depth, total_frames_decoded);
    }

    fn on_qoe_metrics(&mut self, metrics: QoeMetrics) {
        self.hub.on_qoe_metrics(metrics.time_diff_dr);
    }

    fn on_close(&mut self) {
        self.hub.on_close();
    }
}

/// Server capabilities must preserve the peer's `AVC_DISABLED` bit during
/// IronRDP's flag intersection. Mobile Windows App clients advertise that bit
/// because they cannot decode H.264; if the server omits it, negotiation clears
/// it and the server may send AVC420 frames that make the client disconnect.
fn preferred_capabilities() -> Vec<CapabilitySet> {
    vec![
        CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::SMALL_CACHE | CapabilitiesV107Flags::AVC_DISABLED,
        },
        CapabilitySet::V10_6 {
            flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
        },
        CapabilitySet::V10_6Err {
            flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
        },
        CapabilitySet::V10_5 {
            flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
        },
        CapabilitySet::V10_4 {
            flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
        },
        CapabilitySet::V10_3 {
            flags: CapabilitiesV103Flags::AVC_DISABLED,
        },
        CapabilitySet::V10_2 {
            flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
        },
        CapabilitySet::V10 {
            flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
        },
        CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
        },
        CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::SMALL_CACHE,
        },
    ]
}

#[cfg(test)]
fn supports_avc420(capability: &CapabilitySet) -> bool {
    avc_capabilities(capability).1
}

/// Returns (AVC enabled, AVC420 supported, AVC444 supported).
fn avc_capabilities(capability: &CapabilitySet) -> (bool, bool, bool) {
    match capability {
        CapabilitySet::V8 { .. } => (false, false, false),
        CapabilitySet::V8_1 { flags } => {
            let enabled = flags.contains(CapabilitiesV81Flags::AVC420_ENABLED);
            (enabled, enabled, false)
        }
        CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags } => {
            let enabled = !flags.contains(CapabilitiesV10Flags::AVC_DISABLED);
            (enabled, enabled, enabled)
        }
        CapabilitySet::V10_1 => (true, true, true),
        CapabilitySet::V10_3 { flags } => {
            let enabled = !flags.contains(CapabilitiesV103Flags::AVC_DISABLED);
            (enabled, enabled, enabled)
        }
        CapabilitySet::V10_4 { flags }
        | CapabilitySet::V10_5 { flags }
        | CapabilitySet::V10_6 { flags }
        | CapabilitySet::V10_6Err { flags } => {
            let enabled = !flags.contains(CapabilitiesV104Flags::AVC_DISABLED);
            (enabled, enabled, enabled)
        }
        CapabilitySet::V10_7 { flags } => {
            let enabled = !flags.contains(CapabilitiesV107Flags::AVC_DISABLED);
            (enabled, enabled, enabled)
        }
    }
}

/// Whether the selected EGFX capset permits RemoteFX Progressive.
///
/// The final confirmed capability set is authoritative. V8 and V8.1 carry the
/// RemoteFX `THIN_CLIENT` flag, which requires traditional RemoteFX instead of
/// Progressive when set. V10+ `AVC_THIN_CLIENT` applies only to AVC. V10.1 has
/// no flags that establish this codec path, so it is treated conservatively.
fn allows_remotefx_progressive(capability: &CapabilitySet) -> bool {
    match capability {
        CapabilitySet::V8 { flags } => !flags.contains(CapabilitiesV8Flags::THIN_CLIENT),
        CapabilitySet::V8_1 { flags } => !flags.contains(CapabilitiesV81Flags::THIN_CLIENT),
        CapabilitySet::V10 { .. }
        | CapabilitySet::V10_2 { .. }
        | CapabilitySet::V10_3 { .. }
        | CapabilitySet::V10_4 { .. }
        | CapabilitySet::V10_5 { .. }
        | CapabilitySet::V10_6 { .. }
        | CapabilitySet::V10_6Err { .. }
        | CapabilitySet::V10_7 { .. } => true,
        CapabilitySet::V10_1 => false,
    }
}

fn egfx_thin_client(capability: &CapabilitySet) -> Option<bool> {
    match capability {
        CapabilitySet::V8 { flags } => Some(flags.contains(CapabilitiesV8Flags::THIN_CLIENT)),
        CapabilitySet::V8_1 { flags } => Some(flags.contains(CapabilitiesV81Flags::THIN_CLIENT)),
        CapabilitySet::V10_3 { flags } => {
            Some(flags.contains(CapabilitiesV103Flags::AVC_THIN_CLIENT))
        }
        CapabilitySet::V10_4 { flags }
        | CapabilitySet::V10_5 { flags }
        | CapabilitySet::V10_6 { flags }
        | CapabilitySet::V10_6Err { flags } => {
            Some(flags.contains(CapabilitiesV104Flags::AVC_THIN_CLIENT))
        }
        CapabilitySet::V10_7 { flags } => {
            Some(flags.contains(CapabilitiesV107Flags::AVC_THIN_CLIENT))
        }
        CapabilitySet::V10 { .. } | CapabilitySet::V10_1 | CapabilitySet::V10_2 { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_egfx::pdu::{
        CapabilitiesV10Flags, CapabilitiesV81Flags, CapabilitiesV103Flags, CapabilitiesV104Flags,
        CapabilitiesV107Flags,
    };

    use super::{
        NegotiatedCodec, NegotiatedCodecs, allows_remotefx_progressive, preferred_capabilities,
        supports_avc420,
    };

    fn negotiate_common(
        client: &[ironrdp_egfx::pdu::CapabilitySet],
    ) -> Option<ironrdp_egfx::pdu::CapabilitySet> {
        preferred_capabilities().into_iter().find_map(|server| {
            client
                .iter()
                .find(|candidate| candidate.version() == server.version())
                .map(|candidate| match (candidate, &server) {
                    (
                        ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                            flags: client_flags,
                        },
                        ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                            flags: server_flags,
                        },
                    ) => ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                        flags: *client_flags & *server_flags,
                    },
                    (
                        ironrdp_egfx::pdu::CapabilitySet::V10 {
                            flags: client_flags,
                        },
                        ironrdp_egfx::pdu::CapabilitySet::V10 {
                            flags: server_flags,
                        },
                    ) => ironrdp_egfx::pdu::CapabilitySet::V10 {
                        flags: *client_flags & *server_flags,
                    },
                    _ => server,
                })
        })
    }

    #[test]
    fn mobile_client_keeps_avc_disabled_after_server_intersection() {
        let client = [
            ironrdp_egfx::pdu::CapabilitySet::V8 {
                flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::SMALL_CACHE,
            },
            ironrdp_egfx::pdu::CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::SMALL_CACHE,
            },
            ironrdp_egfx::pdu::CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
            },
            ironrdp_egfx::pdu::CapabilitySet::V10_4 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
        ];

        let negotiated = negotiate_common(&client).expect("mobile client shares V10 capability");
        assert!(matches!(
            negotiated,
            ironrdp_egfx::pdu::CapabilitySet::V10_4 { flags }
                if flags.contains(CapabilitiesV104Flags::AVC_DISABLED)
        ));
        assert!(!supports_avc420(&negotiated));
    }

    #[test]
    fn desktop_client_without_avc_disabled_still_negotiates_avc() {
        let client = [ironrdp_egfx::pdu::CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::SMALL_CACHE,
        }];

        let negotiated = negotiate_common(&client).expect("desktop client shares V10.7");
        assert_eq!(
            negotiated,
            ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE,
            }
        );
        assert!(supports_avc420(&negotiated));
    }

    #[test]
    fn every_server_v10_capability_preserves_mobile_avc_disabled() {
        for capability in preferred_capabilities() {
            match capability {
                ironrdp_egfx::pdu::CapabilitySet::V10 { flags }
                | ironrdp_egfx::pdu::CapabilitySet::V10_2 { flags } => {
                    assert!(flags.contains(CapabilitiesV10Flags::AVC_DISABLED));
                }
                ironrdp_egfx::pdu::CapabilitySet::V10_3 { flags } => {
                    assert!(flags.contains(CapabilitiesV103Flags::AVC_DISABLED));
                }
                ironrdp_egfx::pdu::CapabilitySet::V10_4 { flags }
                | ironrdp_egfx::pdu::CapabilitySet::V10_5 { flags }
                | ironrdp_egfx::pdu::CapabilitySet::V10_6 { flags }
                | ironrdp_egfx::pdu::CapabilitySet::V10_6Err { flags } => {
                    assert!(flags.contains(CapabilitiesV104Flags::AVC_DISABLED));
                }
                ironrdp_egfx::pdu::CapabilitySet::V10_7 { flags } => {
                    assert!(flags.contains(CapabilitiesV107Flags::AVC_DISABLED));
                }
                ironrdp_egfx::pdu::CapabilitySet::V8 { .. }
                | ironrdp_egfx::pdu::CapabilitySet::V8_1 { .. }
                | ironrdp_egfx::pdu::CapabilitySet::V10_1 => {}
            }
        }
    }

    #[test]
    fn avc420_follows_the_selected_capability_flags() {
        assert!(!supports_avc420(&ironrdp_egfx::pdu::CapabilitySet::V8 {
            flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::empty(),
        }));
        assert!(supports_avc420(&ironrdp_egfx::pdu::CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED,
        }));
        assert!(!supports_avc420(&ironrdp_egfx::pdu::CapabilitySet::V10_4 {
            flags: CapabilitiesV104Flags::AVC_DISABLED,
        }));
        assert!(supports_avc420(&ironrdp_egfx::pdu::CapabilitySet::V10 {
            flags: CapabilitiesV10Flags::SMALL_CACHE,
        }));
    }

    #[test]
    fn avc_capability_matrix_covers_all_known_versions() {
        use ironrdp_egfx::pdu::{
            CapabilitiesV8Flags, CapabilitiesV103Flags, CapabilitiesV107Flags,
        };

        let cases = [
            (
                ironrdp_egfx::pdu::CapabilitySet::V8 {
                    flags: CapabilitiesV8Flags::empty(),
                },
                false,
                false,
            ),
            (
                ironrdp_egfx::pdu::CapabilitySet::V8_1 {
                    flags: CapabilitiesV81Flags::AVC420_ENABLED,
                },
                true,
                false,
            ),
            (
                ironrdp_egfx::pdu::CapabilitySet::V10 {
                    flags: CapabilitiesV10Flags::empty(),
                },
                true,
                true,
            ),
            (ironrdp_egfx::pdu::CapabilitySet::V10_1, true, true),
            (
                ironrdp_egfx::pdu::CapabilitySet::V10_2 {
                    flags: CapabilitiesV10Flags::empty(),
                },
                true,
                true,
            ),
            (
                ironrdp_egfx::pdu::CapabilitySet::V10_3 {
                    flags: CapabilitiesV103Flags::empty(),
                },
                true,
                true,
            ),
            (
                ironrdp_egfx::pdu::CapabilitySet::V10_4 {
                    flags: CapabilitiesV104Flags::empty(),
                },
                true,
                true,
            ),
            (
                ironrdp_egfx::pdu::CapabilitySet::V10_5 {
                    flags: CapabilitiesV104Flags::empty(),
                },
                true,
                true,
            ),
            (
                ironrdp_egfx::pdu::CapabilitySet::V10_6 {
                    flags: CapabilitiesV104Flags::empty(),
                },
                true,
                true,
            ),
            (
                ironrdp_egfx::pdu::CapabilitySet::V10_6Err {
                    flags: CapabilitiesV104Flags::empty(),
                },
                true,
                true,
            ),
            (
                ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                    flags: CapabilitiesV107Flags::empty(),
                },
                true,
                true,
            ),
        ];

        for (capability, expected_avc420, expected_avc444) in cases {
            let codecs = NegotiatedCodecs::from_capability(&capability, false, false, false);
            assert_eq!(
                codecs.avc420_supported_by_peer, expected_avc420,
                "{capability:?}"
            );
            assert_eq!(
                codecs.avc444_supported_by_peer, expected_avc444,
                "{capability:?}"
            );
            assert_eq!(codecs.capability_version, capability.version().0);
        }
    }

    #[test]
    fn avc444_support_is_implied_by_v10_or_newer_capability_with_avc_enabled() {
        let v10 = ironrdp_egfx::pdu::CapabilitySet::V10 {
            flags: CapabilitiesV10Flags::SMALL_CACHE,
        };
        let disabled = ironrdp_egfx::pdu::CapabilitySet::V10 {
            flags: CapabilitiesV10Flags::AVC_DISABLED,
        };
        let v81 = ironrdp_egfx::pdu::CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED,
        };

        let v10_codecs = NegotiatedCodecs::from_capability(&v10, false, false, false);
        assert!(v10_codecs.avc_enabled);
        assert!(v10_codecs.avc420_supported_by_peer);
        assert!(v10_codecs.avc444_supported_by_peer);
        assert_eq!(v10_codecs.selected, NegotiatedCodec::Avc420);

        let disabled_codecs = NegotiatedCodecs::from_capability(&disabled, true, true, true);
        assert!(!disabled_codecs.avc_enabled);
        assert!(!disabled_codecs.avc420_supported_by_peer);
        assert!(!disabled_codecs.avc444_supported_by_peer);
        assert_eq!(
            disabled_codecs.selected,
            NegotiatedCodec::RemoteFxProgressive
        );

        let v81_codecs = NegotiatedCodecs::from_capability(&v81, true, false, false);
        assert!(v81_codecs.avc_enabled);
        assert!(v81_codecs.avc420_supported_by_peer);
        assert!(!v81_codecs.avc444_supported_by_peer);
        assert_eq!(v81_codecs.selected, NegotiatedCodec::Avc420);
    }

    #[test]
    fn selection_keeps_avc444_disabled_until_dual_stream_encoder_is_implemented() {
        let capability = ironrdp_egfx::pdu::CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::SMALL_CACHE,
        };
        let without_sender = NegotiatedCodecs::from_capability(&capability, false, true, true);
        assert_eq!(without_sender.selected, NegotiatedCodec::Avc420);

        let with_sender = NegotiatedCodecs::from_capability(&capability, true, true, true);
        assert_eq!(with_sender.selected, NegotiatedCodec::Avc444);
    }

    #[test]
    fn selection_falls_back_to_progressive_then_legacy() {
        let capability = ironrdp_egfx::pdu::CapabilitySet::V8 {
            flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::SMALL_CACHE,
        };
        let progressive = NegotiatedCodecs::from_capability(&capability, false, true, true);
        assert_eq!(progressive.selected, NegotiatedCodec::RemoteFxProgressive);

        let legacy = NegotiatedCodecs::from_capability(&capability, false, true, false);
        assert_eq!(legacy.selected, NegotiatedCodec::LegacyBitmap);
    }

    #[test]
    fn capability_set_does_not_claim_progressive_without_peer_evidence() {
        let capability = ironrdp_egfx::pdu::CapabilitySet::V8 {
            flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::SMALL_CACHE,
        };
        let codecs = NegotiatedCodecs::from_capability(&capability, false, false, false);
        assert!(!codecs.progressive_allowed_by_capability);
        assert_eq!(codecs.selected, NegotiatedCodec::LegacyBitmap);
    }

    #[test]
    fn progressive_eligibility_comes_only_from_final_egfx_flags() {
        assert!(allows_remotefx_progressive(
            &ironrdp_egfx::pdu::CapabilitySet::V8 {
                flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::SMALL_CACHE,
            }
        ));
        assert!(!allows_remotefx_progressive(
            &ironrdp_egfx::pdu::CapabilitySet::V8 {
                flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::THIN_CLIENT,
            }
        ));
        assert!(allows_remotefx_progressive(
            &ironrdp_egfx::pdu::CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED,
            }
        ));
        assert!(!allows_remotefx_progressive(
            &ironrdp_egfx::pdu::CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::THIN_CLIENT,
            }
        ));
        assert!(allows_remotefx_progressive(
            &ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::AVC_THIN_CLIENT,
            }
        ));
        assert!(!allows_remotefx_progressive(
            &ironrdp_egfx::pdu::CapabilitySet::V10_1
        ));
    }
}
