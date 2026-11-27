//! End-to-end EGFX tests: AVC420 bytes and RemoteFX Progressive frames reach IronRDP clients
//! through the real TLS/NLA session, are acknowledged, and preserve the negotiated path.

#[path = "support/client.rs"]
mod client;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawtank_core::events::CoreEvent;
use clawtank_core::server::{self, ServerConfig};
use clawtank_core::tls::TlsIdentity;
use clawtank_core::video::SubmitResult;
use ironrdp_async::FramedWrite as _;
use ironrdp_egfx::pdu::{CapabilitiesV104Flags, CapabilitySet};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_session::ActiveStageOutput;
use ironrdp_session::image::DecodedImage;

const USER: &str = "tester";
const PASSWORD: &str = "video-test-password";

/// Not decodable H.264, just Annex-B-shaped bytes (SPS, PPS, IDR slice); the server treats the
/// payload as opaque, and the test client records rather than decodes.
const FAKE_IDR: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x4d, 0x00, 0x2a, 0xab, //
    0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, //
    0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff,
];
const FAKE_P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02, 0x04];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitted_h264_reaches_an_egfx_client() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: TlsIdentity::generate("video-test.local").unwrap(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
    };
    let server = server::start(
        config,
        Arc::new(move |e| log.lock().unwrap().push(e)),
        Arc::new(|_| {}),
    )
    .unwrap();
    let video = Arc::clone(server.video());
    assert_eq!(
        video.submit_h264(FAKE_IDR, true, 1512, 982),
        SubmitResult::NotReady,
        "no session yet"
    );

    let recorder = client::EgfxRecorder::default();
    let (result, mut session, _) = client::connect(
        server.local_addr(),
        USER,
        PASSWORD,
        1512,
        982,
        Some(&recorder),
    )
    .await
    .expect("connect with EGFX");

    // Pump until the server reports the pipeline ready (capability exchange runs over DVC).
    let started = {
        let events = Arc::clone(&events);
        move || {
            events.lock().unwrap().iter().any(|e| {
                matches!(
                    e,
                    CoreEvent::VideoStart {
                        width: 1512,
                        height: 982,
                        ..
                    }
                )
            })
        }
    };
    // `pump_until` consumes the connection result, so build a stage once and keep pumping.
    let ok = client::pump_until(result, &mut session, Duration::from_secs(15), {
        let video = Arc::clone(&video);
        let recorder = recorder.clone();
        let mut submitted = false;
        move || {
            if !started() {
                return false;
            }
            if !submitted {
                // The decoder chain must start at an IDR.
                assert_eq!(
                    video.submit_h264(FAKE_P, false, 1512, 982),
                    SubmitResult::NeedKeyframe
                );
                assert_eq!(
                    video.submit_h264(FAKE_IDR, true, 1512, 982),
                    SubmitResult::Sent
                );
                assert_eq!(
                    video.submit_h264(FAKE_P, false, 1512, 982),
                    SubmitResult::Sent
                );
                submitted = true;
            }
            recorder.frame_count() >= 2
        }
    })
    .await
    .expect("session");
    assert!(ok, "timed out; events: {:?}", events.lock().unwrap());

    let streams = recorder.bitstreams.lock().unwrap().clone();
    assert_eq!(
        streams,
        vec![FAKE_IDR.to_vec(), FAKE_P.to_vec()],
        "bitstreams must arrive unchanged"
    );
    assert_eq!(*recorder.reset_size.lock().unwrap(), Some((1512, 982)));
    // Acks drive adaptive bitrate: the client acknowledges each frame and we time it.
    // The client sent its acks while pumping above; give the server a moment to process them.
    let mut ok = false;
    for _ in 0..100 {
        if video.stats().frames_acked >= 2 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stats = video.stats();
    assert!(ok, "client acknowledged frames: {stats:?}");
    assert_eq!((stats.frames_sent, stats.frames_dropped), (2, 1));
    assert!(stats.ack_latency_us > 0, "ack latency measured: {stats:?}");

    drop(session);
    for _ in 0..100 {
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::VideoStop))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::VideoStop))
    );
    wait_for_session_reset(&video).await;
    assert_eq!(
        video.submit_h264(FAKE_IDR, true, 1512, 982),
        SubmitResult::NotReady,
        "session is gone"
    );
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn avc420_frames_migrate_to_reliable_udp_and_keep_tcp_control_plane() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: TlsIdentity::generate("udp-video-test.local").unwrap(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
    };
    let server = server::start(
        config,
        Arc::new(move |event| log.lock().unwrap().push(event)),
        Arc::new(|_| {}),
    )
    .unwrap();
    let video = Arc::clone(server.video());
    let (width, height) = (200_u16, 200_u16);
    let recorder = client::EgfxRecorder::default();
    let (result, mut framed, _, mut tunnel) = client::connect_with_channels_and_multitransport(
        server.local_addr(),
        client::client_config(USER, PASSWORD, width, height),
        Some(&recorder),
        |_| {},
        true,
    )
    .await
    .expect("connect with reliable UDP multitransport");
    assert!(
        result.multitransport_soft_sync(),
        "client must negotiate Soft-Sync with the server"
    );
    let mut tunnel = tunnel.take().expect("UDP tunnel established");

    let mut stage = client::active_stage(result);
    stage
        .enable_reliable_udp_dvc_tunnel()
        .expect("DRDYNVC is present");
    let mut image = DecodedImage::new(PixelFormat::RgbA32, width, height);

    // Drive the handshake and EGFX channel opening on TCP. After the server
    // sends its first graphics batch it must issue a Soft-Sync Migration
    // Request; until that arrives, all traffic is still on the control plane.
    let migrated = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let (action, payload) = framed.read_pdu().await.expect("read TCP PDU");
            for output in stage
                .process(&mut image, action, &payload, framed.last_read_at())
                .expect("process TCP PDU")
            {
                if let ActiveStageOutput::ResponseFrame(response) = output {
                    framed
                        .write_all(&response)
                        .await
                        .expect("write TCP response");
                }
            }
            let events = events.lock().unwrap();
            let ready = events.iter().any(|event| {
                matches!(
                    event,
                    CoreEvent::VideoStart {
                        width: started_width,
                        height: started_height,
                        ..
                    } if (*started_width, *started_height) == (width, height)
                )
            });
            drop(events);
            if ready && video.submit_h264(FAKE_IDR, true, width, height) == SubmitResult::Sent {
                return true;
            }
        }
    })
    .await
    .expect("video start and IDR submission");
    assert!(migrated);

    // The first EGFX output is sent on TCP and triggers the Migration Request.
    // Continue reading TCP until the client has acknowledged the Soft-Sync
    // response and the server has moved EGFX onto the tunnel.
    let tunnel_in_use = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let read = tokio::time::timeout(Duration::from_millis(200), framed.read_pdu()).await;
            if let Ok(pdu) = read {
                let (action, payload) = pdu.expect("read TCP PDU");
                for output in stage
                    .process(&mut image, action, &payload, framed.last_read_at())
                    .expect("process TCP PDU")
                {
                    if let ActiveStageOutput::ResponseFrame(response) = output {
                        framed
                            .write_all(&response)
                            .await
                            .expect("write TCP response");
                    }
                }
            }
            if stage.reliable_udp_dvc_tunnel_in_use() {
                break;
            }
        }
    })
    .await;
    assert!(tunnel_in_use.is_ok(), "EGFX did not migrate to UDP");

    // A frame submitted after migration must arrive on the tunnel, not the
    // TCP transport. Read tunnel data and feed it through the public DVC
    // tunnel entry point so decoded EGFX updates are observable.
    let frame_submitted = video.submit_h264(FAKE_P, false, width, height);
    assert!(
        matches!(
            frame_submitted,
            SubmitResult::Sent | SubmitResult::NeedKeyframe
        ),
        "post-migration frame was neither queued nor refused for keyframe recovery: {frame_submitted:?}"
    );
    let udp_frame = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let payload = tunnel.recv().await.expect("UDP tunnel remains open");
            let (replies, outputs) = stage
                .process_dvc_tunnel(
                    &mut image,
                    ironrdp_dvc::pdu::SoftSyncTunnelType::RELIABLE_UDP,
                    &payload,
                )
                .expect("process DVC data over UDP");
            for reply in replies.messages() {
                let bytes = reply
                    .encode_unframed_pdu()
                    .expect("encode DVC reply for the tunnel");
                tunnel.send(bytes).await.expect("send DVC reply over UDP");
            }
            if outputs
                .iter()
                .any(|output| matches!(output, ActiveStageOutput::GraphicsUpdate(_)))
            {
                return true;
            }
        }
    })
    .await;
    assert!(udp_frame.is_ok(), "no graphics update arrived over UDP");

    let acked = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if video.stats().frames_acked >= 1 {
                break true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(acked.is_ok(), "UDP frame ACK did not reach the server");
    assert!(
        events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                CoreEvent::VideoTransport {
                    transport: clawtank_core::events::VideoTransport::Udp
                }
            )
        }),
        "server did not report EGFX migration to UDP"
    );

    // Display Control is deliberately not migrated. Sending a resize over the
    // original TCP connection therefore proves the control plane remains usable
    // after EGFX has moved to the sideband transport.
    let resize = loop {
        if let Some(resize) = stage.encode_resize(640, 480, None, None) {
            break resize.expect("encode Display Control resize for TCP");
        }
        framed
            .read_pdu()
            .await
            .expect("read TCP while Display Control becomes ready");
    };
    framed
        .write_all(&resize)
        .await
        .expect("write Display Control resize over TCP");
    let tcp_resize = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if events.lock().unwrap().iter().any(|event| {
                matches!(
                    event,
                    CoreEvent::VideoResize {
                        width: 640,
                        height: 480,
                        ..
                    }
                )
            }) {
                break true;
            }
            let read = tokio::time::timeout(Duration::from_millis(200), framed.read_pdu()).await;
            if let Ok(pdu) = read {
                let (action, payload) = pdu.expect("read TCP after resize");
                for output in stage
                    .process(&mut image, action, &payload, framed.last_read_at())
                    .expect("process TCP resize response")
                {
                    if let ActiveStageOutput::ResponseFrame(response) = output {
                        framed
                            .write_all(&response)
                            .await
                            .expect("write TCP resize response");
                    }
                }
            }
        }
    })
    .await;
    assert!(
        tcp_resize.is_ok(),
        "TCP Display Control did not survive EGFX UDP migration"
    );

    drop(tunnel);
    drop(framed);
    wait_for_video_stop(&events).await;
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_decline_keeps_avc420_on_tcp() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: TlsIdentity::generate("udp-decline-test.local").unwrap(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
    };
    let server = server::start(
        config,
        Arc::new(move |event| log.lock().unwrap().push(event)),
        Arc::new(|_| {}),
    )
    .unwrap();
    let video = Arc::clone(server.video());
    let (width, height) = (200_u16, 200_u16);
    let recorder = client::EgfxRecorder::default();
    let (result, mut framed, _, tunnel) = client::connect_with_channels_and_multitransport(
        server.local_addr(),
        client::client_config(USER, PASSWORD, width, height),
        Some(&recorder),
        |_| {},
        false,
    )
    .await
    .expect("connect without UDP");
    assert!(tunnel.is_none());

    let mut stage = client::active_stage(result);
    let mut image = DecodedImage::new(PixelFormat::RgbA32, width, height);
    let sent = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let read = tokio::time::timeout(Duration::from_millis(200), framed.read_pdu()).await;
            if let Ok(pdu) = read {
                let (action, payload) = pdu.expect("read TCP PDU");
                for output in stage
                    .process(&mut image, action, &payload, framed.last_read_at())
                    .expect("process TCP PDU")
                {
                    if let ActiveStageOutput::ResponseFrame(response) = output {
                        framed
                            .write_all(&response)
                            .await
                            .expect("write TCP response");
                    }
                }
            }
            if recorder.frame_count() >= 1 {
                break true;
            }
            let ready = events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, CoreEvent::VideoStart { .. }));
            if ready {
                let result = video.submit_h264(FAKE_IDR, true, width, height);
                assert_ne!(result, SubmitResult::NotReady);
            }
        }
    })
    .await;
    assert!(sent.is_ok(), "TCP fallback did not deliver an AVC420 frame");
    assert!(
        events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                CoreEvent::VideoTransport {
                    transport: clawtank_core::events::VideoTransport::Tcp
                }
            )
        }),
        "server did not report the TCP fallback"
    );

    drop(framed);
    wait_for_video_stop(&events).await;
    server.stop();
}

async fn wait_for_video_stop(events: &Arc<Mutex<Vec<CoreEvent>>>) {
    for _ in 0..200 {
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, CoreEvent::VideoStop))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for VideoStop; events: {:?}",
        events.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitted_progressive_reaches_avc_disabled_client() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: TlsIdentity::generate("progressive-test.local").unwrap(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
    };
    let server = server::start(
        config,
        Arc::new(move |event| log.lock().unwrap().push(event)),
        Arc::new(|_| {}),
    )
    .unwrap();
    let video = Arc::clone(server.video());
    // The server's minimum side is 200 pixels.
    let (width, height) = (200_u16, 200_u16);

    let recorder = client::EgfxRecorder {
        advertise_avc_disabled: true,
        ..Default::default()
    };
    let (result, mut session, _) = client::connect(
        server.local_addr(),
        USER,
        PASSWORD,
        width,
        height,
        Some(&recorder),
    )
    .await
    .expect("connect with AVC-disabled EGFX");

    let started = {
        let events = Arc::clone(&events);
        move || {
            events.lock().unwrap().iter().any(|event| {
                matches!(
                    event,
                    CoreEvent::ProgressiveStart {
                        width: started_width,
                        height: started_height,
                        ..
                    } if (*started_width, *started_height) == (width, height)
                )
            })
        }
    };
    let frame = clawtank_core::test_pattern::render_bgra(width, height);
    let (ok, result_image) =
        client::pump_until_with_image(result, &mut session, Duration::from_secs(15), {
            let video = Arc::clone(&video);
            let recorder = recorder.clone();
            move || {
                if !started() {
                    return false;
                }
                if recorder.progressive_pdus.load(Ordering::Acquire) == 0 {
                    let result =
                        video.submit_progressive(&frame, width, height, usize::from(width) * 4);
                    if result != SubmitResult::NotReady {
                        assert_eq!(result, SubmitResult::Sent);
                    }
                    return false;
                }
                recorder.frame_count() >= 1
            }
        })
        .await
        .expect("session");
    assert!(ok, "events: {:?}", events.lock().unwrap());
    assert_eq!(
        recorder.negotiated_capability.lock().unwrap().as_ref(),
        Some(&CapabilitySet::V10_4 {
            flags: CapabilitiesV104Flags::AVC_DISABLED,
        })
    );
    assert!(recorder.progressive_pdus.load(Ordering::Acquire) >= 1);
    assert_eq!(*recorder.progressive_codec_context.lock().unwrap(), Some(0));
    assert!(
        recorder.progressive_updates.load(Ordering::Acquire) >= 1,
        "Progressive decoder delivered tessellated bitmap updates"
    );
    let pixels = result_image.data();
    let stride = result_image.stride();
    for (x, y) in [
        (0_u32, 0_u32),
        (199, 0),
        (0, 199),
        (199, 199),
        (100, 100),
        (100, 199),
    ] {
        let index = usize::try_from(y).unwrap() * stride + usize::try_from(x).unwrap() * 4;
        let (r, g, b) =
            clawtank_core::test_pattern::expected_rgb(x, y, u32::from(width), u32::from(height));
        assert!(
            pixels[index..index + 3]
                .iter()
                .zip([r, g, b])
                .all(|(&got, want)| got.abs_diff(want) <= client::CODEC_TOLERANCE),
            "pixel ({x},{y}): got {:?}, want {:?}",
            &pixels[index..index + 3],
            [r, g, b]
        );
    }

    for _ in 0..100 {
        if video.stats().frames_acked >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stats = video.stats();
    assert!(stats.frames_sent >= 1, "{stats:?}");
    assert_eq!(stats.frames_acked, 1, "{stats:?}");
    assert!(stats.ack_latency_us > 0, "{stats:?}");

    server.stop();
}

async fn wait_for_session_reset(video: &clawtank_core::video::VideoHub) {
    for _ in 0..100 {
        if !video.can_accept() && !video.can_accept_bitmap() && !video.can_accept_progressive() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("prior graphics session did not close");
}
