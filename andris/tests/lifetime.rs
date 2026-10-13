//! How long andris serves: until a signal, which stops it cleanly, or
//! until the desktop goes, which stops it with an error. Driven through
//! `andris::serve_until` against an in-process andris on the synthetic
//! platform, with a future standing in for the signal: no real signal is
//! sent inside the test process.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use andris::platform::{EvdevCode, InputEvent};
use shakenfist_spice_protocol::messages::{KeyEvent, WireType};
use shakenfist_spice_protocol::{ChannelType, inputs_client, inputs_server};
use tokio::net::TcpStream;

use support::{TestClient, TestServer, WAIT};

/// How soon serving must stop once the desktop has gone.
const PROMPTLY: Duration = Duration::from_secs(1);

/// `KEY_A`, which scancode 0x1E presses.
const KEY_A: EvdevCode = EvdevCode(30);

/// A main channel, which holds the session open, and an inputs channel
/// past its `INIT`.
async fn open_channels(server: &TestServer) -> (TestClient, TestClient) {
    let (main, session) = server.open_main().await;
    let mut inputs = server.open(session, ChannelType::Inputs).await;
    let init = inputs.recv().await;
    assert_eq!(init.header.message_type, inputs_server::INIT);
    (main, inputs)
}

#[tokio::test]
async fn a_vanished_desktop_stops_the_server_with_an_error() {
    let server = TestServer::start().await;
    let (mut main, mut inputs) = open_channels(&server).await;
    let addr = server.addr();
    let TestServer {
        handle, platform, ..
    } = server;

    let serving = tokio::spawn(andris::serve_until(
        handle,
        std::future::pending(),
        platform.platform().gone,
    ));
    platform.vanish("the test's X server exited").unwrap();
    let result = tokio::time::timeout(PROMPTLY, serving)
        .await
        .expect("serving did not stop within a second of the desktop going")
        .unwrap();
    let message = format!("{:#}", result.expect_err("a vanished desktop is an error"));
    assert!(
        message.contains("the test's X server exited"),
        "the error does not say why: {message}"
    );

    main.expect_closed().await;
    inputs.expect_closed().await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "the listener still accepts connections"
    );
}

// On the current-thread runtime, so that no other task runs between
// serve_until returning and the check of what reached the desktop: a
// release that only dropping the server caused would not be in yet.
#[tokio::test(flavor = "current_thread")]
async fn a_signal_stops_the_server_cleanly_and_releases_held_keys() {
    let server = TestServer::start().await;
    let (mut main, mut inputs) = open_channels(&server).await;
    let mut press = Vec::new();
    KeyEvent { scancode: 0x1E }.write(&mut press);
    inputs.send(inputs_client::KEY_DOWN, &press).await;
    let pressed = InputEvent::Key {
        code: KEY_A,
        pressed: true,
    };
    support::wait_until("the key press reaches the desktop", || {
        server.platform.take_input_events().unwrap() == vec![pressed]
    })
    .await;
    let TestServer {
        handle, platform, ..
    } = server;

    let serving = andris::serve_until(
        handle,
        std::future::ready("SIGTERM"),
        platform.platform().gone,
    );
    tokio::time::timeout(WAIT, serving)
        .await
        .expect("serving did not stop after the signal")
        .expect("a signal is a clean stop");

    // Serving returns only once the release has reached the desktop.
    assert_eq!(
        platform.take_input_events().unwrap(),
        vec![InputEvent::Key {
            code: KEY_A,
            pressed: false
        }]
    );
    main.expect_closed().await;
    inputs.expect_closed().await;
}
