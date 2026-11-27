// Shared by several test binaries and an example; each uses a different subset.
#![allow(dead_code)]

//! Minimal RDP client built on IronRDP's client crates, shared by the integration tests and
//! the `probe` example. It connects with TLS + NLA, optionally joins the EGFX graphics
//! pipeline (recording H.264 and decoded bitmap updates), and either reads legacy updates
//! until the image matches ClawTank's test pattern or pumps the session.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clawtank_core::test_pattern::expected_rgb;
use clawtank_core::tls::TlsIdentity;
use ironrdp_async::FramedWrite as _;
use ironrdp_connector::{ClientConnector, ConnectionResult, ConnectorErrorExt as _, Credentials};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::decode::{DecodedFrame, DecoderResult, H264Decoder};
use ironrdp_egfx::pdu::{CapabilitySet, Codec2Type, WireToSurface2Pdu};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_pdu::gcc::KeyboardType;
use ironrdp_pdu::gcc::{ConnectionType, MultiTransportFlags};
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::rdp::capability_sets::RailSupportLevel;
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_session::ActiveStageBuilder;
pub use ironrdp_session::ActiveStageOutput;
pub use ironrdp_session::image::DecodedImage;
use ironrdp_tls::CertificateValidation;
use ironrdp_tokio::TokioFramed;
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;

/// Max per-channel difference accepted between sent and decoded pixels (the server may pick a
/// lossy codec such as RemoteFX).
pub const CODEC_TOLERANCE: u8 = 8;

pub type Session = ironrdp_tokio::TokioFramed<TlsStream<TcpStream>>;

pub fn client_config(
    user: &str,
    password: &str,
    width: u16,
    height: u16,
) -> ironrdp_connector::Config {
    client_config_with(user, password, width, height, false)
}

/// Same client configuration, named for tests that need the server-drawn
/// pointer disabled so a solid diagnostic bitmap stays visible.
pub fn client_config_without_test_pattern(
    user: &str,
    password: &str,
    width: u16,
    height: u16,
) -> ironrdp_connector::Config {
    client_config_with(user, password, width, height, false)
}

/// `server_pointer`: ask the server for cursor shapes (client-side cursor rendering).
pub fn client_config_with(
    user: &str,
    password: &str,
    width: u16,
    height: u16,
    server_pointer: bool,
) -> ironrdp_connector::Config {
    ironrdp_connector::Config {
        desktop_size: ironrdp_connector::DesktopSize { width, height },
        monitor_layout: None,
        desktop_scale_factor: 0,
        desktop_physical_size: None,
        desktop_orientation: None,
        device_scale_factor: None,
        monitor_extended_layout: None,
        relative_mouse_input: false,
        enable_tls: false,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        credentials: Credentials::UsernamePassword {
            username: user.to_owned(),
            password: password.to_owned(),
        },
        domain: None,
        client_build: 0,
        client_name: "clawtank-probe".to_owned(),
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        connection_type: ConnectionType::Lan,
        ime_file_name: String::new(),
        bitmap: None,
        dig_product_id: String::new(),
        client_dir: String::new(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform: MajorPlatformType::MACINTOSH,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        enable_audio_capture: false,
        performance_flags: PerformanceFlags::default(),
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: None,
        enable_server_pointer: server_pointer,
        pointer_software_rendering: !server_pointer,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: false,
        rdp_version: ironrdp_pdu::gcc::RdpVersion::V5_PLUS,
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::SUPPORTED,
    }
}

/// What the EGFX side of the client observed. Cheap to clone; shared with the channel.
#[derive(Clone, Default)]
pub struct EgfxRecorder {
    /// The first few AVC420 bitstreams received (Annex-B bytes as sent by the server).
    pub bitstreams: Arc<Mutex<Vec<Vec<u8>>>>,
    pub bytes: Arc<AtomicU64>,
    /// EndFrame count (frames fully delivered, whatever the codec).
    pub frames: Arc<AtomicU64>,
    /// Size from the last ResetGraphics.
    pub reset_size: Arc<Mutex<Option<(u32, u32)>>>,
    /// Progressive payload metadata observed on `WireToSurface2`.
    pub progressive_pdus: Arc<AtomicU64>,
    pub progressive_bytes: Arc<AtomicU64>,
    pub progressive_codec_context: Arc<Mutex<Option<u32>>>,
    pub negotiated_capability: Arc<Mutex<Option<CapabilitySet>>>,
    pub progressive_updates: Arc<AtomicU64>,
    pub advertise_avc_disabled: bool,
}

impl EgfxRecorder {
    pub fn frame_count(&self) -> u64 {
        self.frames.load(Ordering::Acquire)
    }

    pub fn byte_count(&self) -> u64 {
        self.bytes.load(Ordering::Acquire)
    }
}

struct RecordingHandler(EgfxRecorder);

impl GraphicsPipelineHandler for RecordingHandler {
    fn capabilities(&self) -> Vec<CapabilitySet> {
        if self.0.advertise_avc_disabled {
            vec![CapabilitySet::V10_4 {
                flags: ironrdp_egfx::pdu::CapabilitiesV104Flags::AVC_DISABLED,
            }]
        } else {
            vec![
                CapabilitySet::V10_4 {
                    flags: ironrdp_egfx::pdu::CapabilitiesV104Flags::empty(),
                },
                CapabilitySet::V8_1 {
                    flags: ironrdp_egfx::pdu::CapabilitiesV81Flags::AVC420_ENABLED,
                },
            ]
        }
    }

    fn on_capabilities_confirmed(&mut self, capability: &CapabilitySet) {
        *self.0.negotiated_capability.lock().unwrap() = Some(capability.clone());
    }
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        *self.0.reset_size.lock().unwrap() = Some((width, height));
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        self.0.frames.fetch_add(1, Ordering::AcqRel);
    }

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        if update.codec_id == ironrdp_egfx::pdu::Codec1Type::Uncompressed {
            self.0.progressive_updates.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        if pdu.codec_id == Codec2Type::RemoteFxProgressive {
            self.0.progressive_pdus.fetch_add(1, Ordering::AcqRel);
            self.0
                .progressive_bytes
                .fetch_add(pdu.bitmap_data.len() as u64, Ordering::AcqRel);
            *self.0.progressive_codec_context.lock().unwrap() = Some(pdu.codec_context_id);
        }
    }
}

/// Stands in for an H.264 decoder: records the bitstream and returns a blank frame.
struct RecordingDecoder(EgfxRecorder);

impl H264Decoder for RecordingDecoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.0.bytes.fetch_add(data.len() as u64, Ordering::AcqRel);
        let mut streams = self.0.bitstreams.lock().unwrap();
        // Keep memory bounded on long probe runs: only the first few are kept for inspection.
        if streams.len() < 8 {
            streams.push(data.to_vec());
        }
        let (w, h) = self.0.reset_size.lock().unwrap().unwrap_or((8192, 8192));
        let (w, h) = (w.div_ceil(16) * 16, h.div_ceil(16) * 16);
        Ok(DecodedFrame::new(vec![0; (w * h * 4) as usize], w, h))
    }

    fn reset(&mut self) {}
}

/// Connects with TLS + NLA. Returns the negotiated session and the server certificate DER.
/// With `egfx`, the client also offers the Graphics Pipeline (AVC420) and records into it.
pub async fn connect(
    addr: SocketAddr,
    user: &str,
    password: &str,
    width: u16,
    height: u16,
    egfx: Option<&EgfxRecorder>,
) -> anyhow::Result<(ConnectionResult, Session, Vec<u8>)> {
    connect_with(addr, client_config(user, password, width, height), egfx).await
}

/// Like [`connect`], with a caller-built configuration.
pub async fn connect_with(
    addr: SocketAddr,
    config: ironrdp_connector::Config,
    egfx: Option<&EgfxRecorder>,
) -> anyhow::Result<(ConnectionResult, Session, Vec<u8>)> {
    connect_with_channels(addr, config, egfx, |_| {}).await
}

/// Like [`connect_with`]; `attach` can add more static channels (audio, clipboard, …).
pub async fn connect_with_channels(
    addr: SocketAddr,
    config: ironrdp_connector::Config,
    egfx: Option<&EgfxRecorder>,
    attach: impl FnOnce(&mut ClientConnector),
) -> anyhow::Result<(ConnectionResult, Session, Vec<u8>)> {
    connect_with_channels_and_multitransport(addr, config, egfx, attach, false)
        .await
        .map(|(result, framed, cert, _)| (result, framed, cert))
}

/// Like [`connect_with_channels`], but advertises reliable UDP Soft-Sync and
/// establishes the sideband transport before finalization completes.
pub async fn connect_with_channels_and_multitransport(
    addr: SocketAddr,
    config: ironrdp_connector::Config,
    egfx: Option<&EgfxRecorder>,
    attach: impl FnOnce(&mut ClientConnector),
    use_udp: bool,
) -> anyhow::Result<(
    ConnectionResult,
    Session,
    Vec<u8>,
    Option<ironrdp_rdpeudp_tokio::UdpTransport>,
)> {
    let server_name = "clawtank-server";
    let tcp = TcpStream::connect(addr).await?;
    // Like real RDP clients: small PDUs (input, file-range requests) must not wait for Nagle.
    tcp.set_nodelay(true)?;
    let client_addr = tcp.local_addr()?;
    let mut config = config;
    config.support_dyn_vc_gfx_protocol = egfx.is_some();
    if use_udp {
        config.multitransport_flags = Some(
            MultiTransportFlags::TRANSPORT_TYPE_UDP_FECR
                | MultiTransportFlags::SOFT_SYNC_TCP_TO_UDP,
        );
    }
    let mut framed = TokioFramed::new(tcp);
    let mut connector = ClientConnector::new(config, client_addr);
    if let Some(recorder) = egfx {
        let gfx = GraphicsPipelineClient::new(
            Box::new(RecordingHandler(recorder.clone())),
            Some(Box::new(RecordingDecoder(recorder.clone()))),
        );
        // Display Control too, so tests can request resizes (ActiveStage::encode_resize).
        let display_control = DisplayControlClient::new(|_| Ok(Vec::new()));
        connector.attach_static_channel(
            DrdynvcClient::new()
                .with_dynamic_channel(gfx)
                .with_dynamic_channel(display_control),
        );
    }

    attach(&mut connector);
    let should_upgrade = ironrdp_async::connect_begin(&mut framed, &mut connector).await?;
    let tcp = framed.into_inner_no_leftover();

    let tls_config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
    .with_no_client_auth();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_config))
        .connect(server_name.try_into()?, tcp)
        .await?;
    let cert_der = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .ok_or_else(|| anyhow::anyhow!("no server certificate"))?
        .to_vec();
    let server_public_key = TlsIdentity {
        cert_der: cert_der.clone(),
        key_pkcs8_der: Vec::new(),
    }
    .public_key()
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let upgraded = ironrdp_async::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = TokioFramed::new(tls);
    let mut udp_probe = None;
    let outcome = ironrdp_async::connect_finalize_with_multitransport(
        upgraded,
        connector,
        &mut framed,
        &mut ReqwestNetworkClient::new(),
        server_name.into(),
        server_public_key,
        None,
        async |request, soft_sync| {
            if !use_udp {
                return Ok(ironrdp_connector::MultitransportResult::Failure(
                    ironrdp_pdu::rdp::multitransport::MultitransportResponsePdu::E_ABORT,
                ));
            }
            assert!(soft_sync, "server and client must negotiate Soft-Sync");
            let mut bootstrap = ironrdp_rdpeudp_tokio::MultitransportBootstrap::new(request);
            let udp_addr = SocketAddr::new(addr.ip(), addr.port());
            let tls = ironrdp_rdpeudp_tokio::UdpTlsConfig {
                certificate_validation: CertificateValidation::DangerouslyAcceptInvalidCertificate,
                ..ironrdp_rdpeudp_tokio::UdpTlsConfig::new("clawtank-server".into())
            };
            bootstrap
                .connect(
                    udp_addr,
                    "clawtank-server".into(),
                    ironrdp_rdpeudp::ConnectionConfig::default(),
                    tls,
                )
                .await
                .map_err(|error| ironrdp_connector::ConnectorError::custom("UDP tunnel", error))?;
            udp_probe = bootstrap.take_transport();
            Ok(ironrdp_connector::MultitransportResult::Success)
        },
    )
    .await?;
    let result = outcome.into_connected()?;
    Ok((result, framed, cert_der, udp_probe))
}

pub fn active_stage(result: ConnectionResult) -> ironrdp_session::ActiveStage {
    ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: None,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build()
}

/// Drives the session (answering the server, feeding channels) until `done()` returns true,
/// the server ends the session, or `timeout` passes. Returns whether `done()` was reached.
pub async fn pump_until(
    result: ConnectionResult,
    framed: &mut Session,
    timeout: Duration,
    mut done: impl FnMut() -> bool,
) -> anyhow::Result<bool> {
    pump_until_with_image(result, framed, timeout, &mut done)
        .await
        .map(|(done, _)| done)
}

/// Like [`pump_until`], but returns the decoded framebuffer as well.
pub async fn pump_until_with_image(
    result: ConnectionResult,
    framed: &mut Session,
    timeout: Duration,
    mut done: impl FnMut() -> bool,
) -> anyhow::Result<(bool, DecodedImage)> {
    let (width, height) = (result.desktop_size.width, result.desktop_size.height);
    let mut stage = active_stage(result);
    let mut image = DecodedImage::new(PixelFormat::RgbA32, width, height);
    let deadline = Instant::now() + timeout;
    while !done() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok((false, image));
        }
        // Wake periodically so `done()` is re-checked even when the server is quiet.
        let read =
            tokio::time::timeout(remaining.min(Duration::from_millis(200)), framed.read_pdu())
                .await;
        let Ok(read) = read else { continue };
        let (action, payload) = read?;
        for out in stage.process(&mut image, action, &payload, framed.last_read_at())? {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await?,
                ActiveStageOutput::Terminate(reason) => {
                    anyhow::bail!("server terminated: {reason:?}")
                }
                _ => {}
            }
        }
    }
    Ok((true, image))
}

/// Reads graphics updates until the image matches the test pattern, or `timeout` passes.
/// On timeout the error describes the sampled pixels. Returns the decoded RGBA image.
pub async fn receive_test_pattern(
    result: ConnectionResult,
    framed: &mut Session,
    timeout: Duration,
) -> anyhow::Result<DecodedImage> {
    let (width, height) = (result.desktop_size.width, result.desktop_size.height);
    let mut stage = active_stage(result);
    let mut image = DecodedImage::new(PixelFormat::RgbA32, width, height);
    let mut updates = 0u32;

    let outcome = tokio::time::timeout(timeout, async {
        loop {
            let (action, payload) = framed.read_pdu().await?;
            for out in stage.process(&mut image, action, &payload, framed.last_read_at())? {
                match out {
                    ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await?,
                    ActiveStageOutput::GraphicsUpdate(_) => {
                        updates += 1;
                        if frame_matches(&image) {
                            return anyhow::Ok(());
                        }
                    }
                    ActiveStageOutput::Terminate(reason) => {
                        anyhow::bail!("server terminated: {reason:?}")
                    }
                    _ => {}
                }
            }
        }
    })
    .await;
    match outcome {
        Ok(Ok(())) => Ok(image),
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!(
            "no matching frame within {timeout:?} after {updates} graphics updates; sampled: {}",
            describe_samples(&image)
        ),
    }
}

fn sample_points(w: u32, h: u32) -> [(u32, u32); 6] {
    [
        (5, 5),
        (w / 2, h / 3),
        (w - 5, 10),
        (10, h - 5),
        (w / 2, h - 5),
        (w - 5, h - 5),
    ]
}

pub fn frame_matches(image: &DecodedImage) -> bool {
    let (w, h) = (u32::from(image.width()), u32::from(image.height()));
    let (data, stride) = (image.data(), image.stride());
    sample_points(w, h).into_iter().all(|(x, y)| {
        let i = y as usize * stride + x as usize * 4;
        let (r, g, b) = expected_rgb(x, y, w, h);
        data[i..i + 3]
            .iter()
            .zip([r, g, b])
            .all(|(&got, want)| got.abs_diff(want) <= CODEC_TOLERANCE)
    })
}

/// "(x,y) got [r,g,b,a] want (r,g,b)" for each sample point, to diagnose mismatches.
pub fn describe_samples(image: &DecodedImage) -> String {
    let (w, h) = (u32::from(image.width()), u32::from(image.height()));
    let (data, stride) = (image.data(), image.stride());
    sample_points(w, h)
        .into_iter()
        .map(|(x, y)| {
            let i = y as usize * stride + x as usize * 4;
            format!(
                "({x},{y}) got {:?} want {:?}",
                &data[i..i + 4],
                expected_rgb(x, y, w, h)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Trusts the self-signed server certificate (real clients prompt the user instead).
#[derive(Debug)]
pub struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algs = rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algs = rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
