//! End-to-end test: a real RDP client (IronRDP's client stack) connects to the ClawTank core
//! over TCP with TLS + NLA, receives the test frame, and disconnects. Also checks that a wrong
//! password is refused and that stopping the server releases the port.

#[path = "support/client.rs"]
mod client;

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawtank_core::events::CoreEvent;
use clawtank_core::server::{self, ServerConfig, ServerHandle};
use clawtank_core::test_pattern;
use clawtank_core::tls::TlsIdentity;
use ironrdp_async::FramedWrite;

const USER: &str = "tester";
const PASSWORD: &str = "correct horse battery staple";
const WIDTH: u16 = 1024;
const HEIGHT: u16 = 768;
const ANDROID_WIDTH: u16 = 2316;
const ANDROID_HEIGHT: u16 = 1080;

type Events = Arc<Mutex<Vec<CoreEvent>>>;

fn start_server(bind: SocketAddr) -> (ServerHandle, Events, TlsIdentity) {
    let events: Events = Arc::default();
    let log = Arc::clone(&events);
    let identity = TlsIdentity::generate("clawtank-test.local").unwrap();
    let config = ServerConfig {
        bind,
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: identity.clone(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
    };
    let handle = server::start(
        config,
        Arc::new(move |e| log.lock().unwrap().push(e)),
        Arc::new(|_| {}),
    )
    .unwrap();
    (handle, events, identity)
}

fn start_server_with_initial_frame(
    bind: SocketAddr,
    width: u16,
    height: u16,
) -> (ServerHandle, Events, TlsIdentity) {
    let events: Events = Arc::default();
    let log = Arc::clone(&events);
    let identity = TlsIdentity::generate("clawtank-test.local").unwrap();
    let config = ServerConfig {
        bind,
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: identity.clone(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
    };
    let handle = server::start_with_initial_bgra_frame(
        config,
        Arc::new(move |e| log.lock().unwrap().push(e)),
        Arc::new(|_| {}),
        test_pattern::render_screen_capture_bgra(width, height),
    )
    .unwrap();
    (handle, events, identity)
}

async fn wait_for(events: &Events, what: &str, pred: impl Fn(&CoreEvent) -> bool) {
    for _ in 0..200 {
        if events.lock().unwrap().iter().any(&pred) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for {what}; events: {:?}",
        events.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nla_client_receives_test_frame_and_disconnects() {
    let (server, events, identity) = start_server("127.0.0.1:0".parse().unwrap());
    let addr = server.local_addr();
    wait_for(&events, "Listening", |e| {
        matches!(e, CoreEvent::Listening(_))
    })
    .await;

    let (result, mut session, cert) = client::connect(addr, USER, PASSWORD, WIDTH, HEIGHT, None)
        .await
        .expect("NLA connection");
    assert_eq!(
        cert, identity.cert_der,
        "server must present the configured certificate"
    );
    assert_eq!(
        (result.desktop_size.width, result.desktop_size.height),
        (WIDTH, HEIGHT)
    );
    wait_for(&events, "ClientConnected", |e| {
        matches!(e, CoreEvent::ClientConnected(_))
    })
    .await;

    client::receive_test_pattern(result, &mut session, Duration::from_secs(20))
        .await
        .expect("test pattern");

    drop(session);
    wait_for(&events, "ClientDisconnected", |e| {
        matches!(e, CoreEvent::ClientDisconnected { .. })
    })
    .await;
    server.stop();
    assert_eq!(events.lock().unwrap().last(), Some(&CoreEvent::Stopped));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_supplied_initial_diagnostic_bitmap_replaces_the_test_pattern() {
    let (server, events, identity) = start_server("127.0.0.1:0".parse().unwrap());
    let addr = server.local_addr();
    wait_for(&events, "Listening", |event| {
        matches!(event, CoreEvent::Listening(_))
    })
    .await;
    let diagnostic = test_pattern::render_bgra(320, 200);
    let video = Arc::clone(server.video());
    let host = tokio::spawn(async move {
        for _ in 0..200 {
            if events.lock().unwrap().iter().any(|event| {
                matches!(
                    event,
                    CoreEvent::InitialScreen {
                        width: 320,
                        height: 200
                    }
                )
            }) {
                assert!(video.submit_initial_bitmap(320, 200, diagnostic));
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("initial-screen event was not emitted");
    });

    let (result, mut session, _) = tokio::time::timeout(
        Duration::from_secs(20),
        client::connect_with(
            addr,
            client::client_config_without_test_pattern(USER, PASSWORD, 320, 200),
            None,
        ),
    )
    .await
    .expect("client connects")
    .expect("connection succeeds");
    host.await
        .expect("host diagnostic frame submission completed");
    let mut image = client::DecodedImage::new(
        ironrdp_graphics::image_processing::PixelFormat::RgbA32,
        320,
        200,
    );
    let mut stage = client::active_stage(result);
    let got_update = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (action, payload) = session.read_pdu().await?;
            for output in stage.process(&mut image, action, &payload, session.last_read_at())? {
                match output {
                    client::ActiveStageOutput::ResponseFrame(frame) => {
                        session.write_all(&frame).await?;
                    }
                    client::ActiveStageOutput::GraphicsUpdate(_) => return anyhow::Ok(()),
                    client::ActiveStageOutput::Terminate(reason) => {
                        anyhow::bail!("server terminated: {reason:?}")
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("initial diagnostic bitmap reaches the client");
    got_update.expect("client accepted the diagnostic bitmap");
    assert_eq!((image.width(), image.height()), (320, 200));
    drop(identity);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_bgra_cyan_survives_wire_rgb_decode() {
    let (server, events, _) =
        start_server_with_initial_frame("127.0.0.1:0".parse().unwrap(), WIDTH, HEIGHT);
    let addr = server.local_addr();
    wait_for(&events, "Listening", |e| {
        matches!(e, CoreEvent::Listening(_))
    })
    .await;

    let (result, mut session, _) = client::connect(addr, USER, PASSWORD, WIDTH, HEIGHT, None)
        .await
        .expect("NLA connection");
    wait_for(&events, "ClientConnected", |e| {
        matches!(e, CoreEvent::ClientConnected(_))
    })
    .await;

    let negotiated_width = u32::from(result.desktop_size.width);
    let image = client::receive_test_pattern(result, &mut session, Duration::from_secs(20))
        .await
        .expect("decoded first legacy bitmap frame");
    // The cyan bar spans [2/8, 3/8) of the pattern. Sample its centre, not
    // the boundary at width / 8, which belongs to the preceding yellow bar.
    let x = negotiated_width * 5 / 16;
    let y = 5u32;
    let i = y as usize * image.stride() + x as usize * 4;
    assert_eq!(
        &image.data()[i..i + 3],
        &[0, 255, 255],
        "BGRA cyan must decode as RGB cyan through the legacy RDP6 wire path"
    );

    drop(session);
    wait_for(&events, "ClientDisconnected", |e| {
        matches!(e, CoreEvent::ClientDisconnected { .. })
    })
    .await;
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nla_client_decodes_android_sized_bitmap_frame() {
    let (server, events, identity) = start_server("127.0.0.1:0".parse().unwrap());
    let addr = server.local_addr();
    wait_for(&events, "Listening", |e| {
        matches!(e, CoreEvent::Listening(_))
    })
    .await;

    let (result, mut session, cert) =
        client::connect(addr, USER, PASSWORD, ANDROID_WIDTH, ANDROID_HEIGHT, None)
            .await
            .expect("NLA connection at Android client dimensions");
    assert_eq!(cert, identity.cert_der);
    assert_eq!(
        (result.desktop_size.width, result.desktop_size.height),
        (ANDROID_WIDTH, ANDROID_HEIGHT)
    );
    wait_for(&events, "ClientConnected", |e| {
        matches!(e, CoreEvent::ClientConnected(_))
    })
    .await;

    client::receive_test_pattern(result, &mut session, Duration::from_secs(20))
        .await
        .expect("IronRDP client decodes the 2316x1080 bitmap frame");

    drop(session);
    wait_for(&events, "ClientDisconnected", |e| {
        matches!(e, CoreEvent::ClientDisconnected { .. })
    })
    .await;
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_is_refused() {
    let (server, events, _) = start_server("127.0.0.1:0".parse().unwrap());
    let addr = server.local_addr();

    let attempt = client::connect(addr, USER, "wrong password", WIDTH, HEIGHT, None).await;
    assert!(attempt.is_err(), "bad password must fail NLA");
    wait_for(&events, "ConnectionFailed", |e| {
        matches!(e, CoreEvent::ConnectionFailed { .. })
    })
    .await;
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::ClientConnected(_))),
        "session must not start"
    );
    server.stop();
}

#[test]
fn stop_releases_the_port() {
    let (server, events, _) = start_server("127.0.0.1:0".parse().unwrap());
    let addr = server.local_addr();
    server.stop();
    assert_eq!(events.lock().unwrap().last(), Some(&CoreEvent::Stopped));
    StdTcpListener::bind(addr).expect("port must be free after stop");
}
