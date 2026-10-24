//! The main channel: its opening messages, the channel list, the mouse
//! mode, and what a replaced client is told, against an in-process andris
//! on the synthetic platform.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use andris::server::main_channel::FAREWELL_TIMEOUT;
use andris::server::session::MouseMode;
use shakenfist_spice_protocol::messages::{
    ChannelEntry, ChannelsList, Disconnecting, MainInit, MainMouseMode, MouseModeRequest, Notify,
    Ping, Pong, ReceivedMessage, WireType,
};
use shakenfist_spice_protocol::{
    ChannelType, MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER, NotifySeverity, SpiceError, SpiceVisibility,
    inputs_server, main_client, main_server,
};

use support::{TestClient, TestServer, closed_within, wait_until};

fn body<T: WireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write(&mut out);
    out
}

/// Read `INIT` and the first `PING`.
async fn skip_opening(client: &mut TestClient) {
    client.recv().await;
    client.recv().await;
}

async fn expect_mouse_mode(client: &mut TestClient, current: u32) {
    let message: ReceivedMessage = client.recv().await;
    assert_eq!(message.header.message_type, main_server::MOUSE_MODE);
    let mode = MainMouseMode::decode(message.payload()).unwrap();
    assert_eq!(
        mode,
        MainMouseMode {
            supported_modes: (MOUSE_MODE_SERVER | MOUSE_MODE_CLIENT) as u16,
            current_mode: current as u16,
        }
    );
}

#[tokio::test]
async fn init_comes_first_with_the_documented_values() {
    let server = TestServer::start().await;
    let (mut client, session) = server.open_main().await;

    let message = client.recv().await;
    assert_eq!(message.header.message_type, main_server::INIT);
    let init = MainInit::decode(message.payload()).unwrap();
    assert_eq!(init.session_id, session);
    assert_eq!(init.display_channels_hint, 1);
    assert_eq!(
        init.supported_mouse_modes,
        MOUSE_MODE_SERVER | MOUSE_MODE_CLIENT
    );
    assert_eq!(init.current_mouse_mode, MOUSE_MODE_CLIENT);
    // Andris plays the guest agent itself, so one is always connected,
    // with spice-server's window of tokens.
    assert_eq!(init.agent_connected, 1);
    assert_eq!(init.agent_tokens, 10);
    assert_eq!(init.ram_hint, 0);
    // Milliseconds since the session began: a fresh session is young.
    assert!(init.multi_media_time < 20_000, "{}", init.multi_media_time);
}

#[tokio::test]
async fn ping_one_follows_init() {
    let server = TestServer::start().await;
    let (mut client, _) = server.open_main().await;
    client.recv().await;

    let message = client.recv().await;
    assert_eq!(message.header.message_type, main_server::PING);
    let ping = Ping::decode(message.payload()).unwrap();
    assert_eq!(ping.id, 1);
}

/// Inputs comes last because ryll opens nothing listed after it; see
/// `andris::server::main_channel::CHANNELS`. The synthetic platform plays
/// a tone, so playback is listed; tests/playback.rs checks that it is not
/// without audio.
#[tokio::test]
async fn attach_channels_lists_display_cursor_playback_and_inputs_last() {
    let server = TestServer::start().await;
    let (mut client, _) = server.open_main().await;
    skip_opening(&mut client).await;

    client.send(main_client::ATTACH_CHANNELS, &[]).await;
    let message = client.recv().await;
    assert_eq!(message.header.message_type, main_server::CHANNELS_LIST);
    let list = ChannelsList::decode(message.payload()).unwrap();
    let entry = |channel_type: ChannelType| ChannelEntry {
        channel_type: channel_type as u8,
        channel_id: 0,
    };
    assert_eq!(
        list.channels,
        vec![
            entry(ChannelType::Display),
            entry(ChannelType::Cursor),
            entry(ChannelType::Playback),
            entry(ChannelType::Inputs),
        ]
    );
}

#[tokio::test]
async fn server_mouse_mode_request_is_applied() {
    let server = TestServer::start().await;
    let (mut client, _) = server.open_main().await;
    skip_opening(&mut client).await;
    let session = server.handle.session().unwrap();
    assert_eq!(*session.mouse_mode().borrow(), MouseMode::Client);

    let request = MouseModeRequest {
        mode: MOUSE_MODE_SERVER as u16,
    };
    client
        .send(main_client::MOUSE_MODE_REQUEST, &body(&request))
        .await;
    expect_mouse_mode(&mut client, MOUSE_MODE_SERVER).await;
    assert_eq!(*session.mouse_mode().borrow(), MouseMode::Server);
}

#[tokio::test]
async fn unsupported_mouse_mode_leaves_the_mode_unchanged() {
    let server = TestServer::start().await;
    let (mut client, _) = server.open_main().await;
    skip_opening(&mut client).await;
    let session = server.handle.session().unwrap();

    for mode in [0u16, 3, 99] {
        client
            .send(
                main_client::MOUSE_MODE_REQUEST,
                &body(&MouseModeRequest { mode }),
            )
            .await;
        expect_mouse_mode(&mut client, MOUSE_MODE_CLIENT).await;
        assert_eq!(*session.mouse_mode().borrow(), MouseMode::Client);
    }
}

#[tokio::test]
async fn unknown_opcode_does_not_close_the_channel() {
    let server = TestServer::start().await;
    let (mut client, _) = server.open_main().await;
    skip_opening(&mut client).await;

    client.send(60000, &[1, 2, 3]).await;
    client.send(main_client::CLIENT_INFO, &[0; 8]).await;
    // The channel still answers.
    client.send(main_client::ATTACH_CHANNELS, &[]).await;
    let message = client.recv().await;
    assert_eq!(message.header.message_type, main_server::CHANNELS_LIST);
}

/// A `MOUSE_MODE_REQUEST` too short to decode closes the main channel,
/// and so ends the session.
#[tokio::test]
async fn malformed_mouse_mode_request_closes_the_main_channel() {
    let server = TestServer::start().await;
    let (mut client, _) = server.open_main().await;
    skip_opening(&mut client).await;

    client.send(main_client::MOUSE_MODE_REQUEST, &[1]).await;
    client.expect_closed().await;
    server.wait_for_no_session().await;
}

#[tokio::test]
async fn the_ping_round_trip_is_recorded() {
    let server = TestServer::start().await;
    let (mut client, _) = server.open_main().await;
    client.recv().await;
    let message = client.recv().await;
    assert_eq!(message.header.message_type, main_server::PING);
    let ping = Ping::decode(message.payload()).unwrap();
    let session = server.handle.session().unwrap();
    assert_eq!(session.stats().ping.count, 0);

    let pong = Pong {
        id: ping.id,
        timestamp: ping.timestamp,
    };
    client.send(main_client::PONG, &body(&pong)).await;
    wait_until("the round trip is recorded", || {
        session.stats().ping.count == 1
    })
    .await;
    let stats = session.stats();
    assert!(stats.ping.latest().is_some());
    assert_eq!(stats.ping.mean(), stats.ping.latest());
    // The main channel's own writes (INIT and PING) are counted.
    assert!(stats.main_bytes > 0);
}

/// A client's main channel past its opening, and its inputs channel past
/// `INIT`: an inputs channel closes only when its session ends.
async fn open_session(server: &TestServer) -> (TestClient, TestClient) {
    let (mut main, session) = server.open_main().await;
    skip_opening(&mut main).await;
    let mut inputs = server.open(session, ChannelType::Inputs).await;
    let init = inputs.recv().await;
    assert_eq!(init.header.message_type, inputs_server::INIT);
    (main, inputs)
}

/// Read the farewell a replaced client is sent: a `NOTIFY` that says who
/// took over, then `DISCONNECTING` with reason `OK`.
async fn expect_farewell(main: &mut TestClient) {
    let message = main.recv().await;
    assert_eq!(message.header.message_type, main_server::NOTIFY);
    let notify = Notify::decode(message.payload()).unwrap();
    assert_eq!(notify.severity_kind(), NotifySeverity::Warn);
    assert_eq!(notify.visibility_kind(), Some(SpiceVisibility::High));
    let text = notify.message_text();
    assert!(
        text.starts_with("another client (127.0.0.1:")
            && text.ends_with(") took over this session"),
        "the notify does not say who took over: {text}"
    );

    let message = main.recv().await;
    assert_eq!(message.header.message_type, main_server::DISCONNECTING);
    let disconnecting = Disconnecting::decode(message.payload()).unwrap();
    assert_eq!(disconnecting.reason_kind(), SpiceError::Ok);
}

/// A replaced client is told why, then its main channel is closed, and
/// its session ends as soon as the client closes it in turn: well within
/// the farewell's time limit.
#[tokio::test]
async fn a_replaced_client_is_told_why_before_its_session_ends() {
    let server = TestServer::start().await;
    let (mut old_main, mut old_inputs) = open_session(&server).await;
    let old = server.session_id();

    let (_new_main, new) = server.open_main().await;
    assert_ne!(new, old);
    expect_farewell(&mut old_main).await;
    old_main.expect_closed().await;
    assert!(
        !closed_within(old_inputs.stream_mut(), Duration::from_millis(200)).await,
        "the old session ended before its client closed the main channel"
    );

    drop(old_main);
    assert!(
        closed_within(old_inputs.stream_mut(), FAREWELL_TIMEOUT / 2).await,
        "the old session outlived its main channel"
    );
    assert_eq!(server.session_id(), new);
}

/// A replaced client that keeps its main channel open after the farewell
/// does not keep the old session: it ends once the farewell's time is up.
#[tokio::test]
async fn a_replaced_client_that_lingers_is_dropped_after_the_farewell() {
    let server = TestServer::start().await;
    let (mut old_main, mut old_inputs) = open_session(&server).await;

    let (_new_main, _) = server.open_main().await;
    expect_farewell(&mut old_main).await;
    assert!(
        !closed_within(old_inputs.stream_mut(), FAREWELL_TIMEOUT / 4).await,
        "the old session ended before the farewell's time was up"
    );
    assert!(
        closed_within(old_inputs.stream_mut(), FAREWELL_TIMEOUT * 2).await,
        "the old session outlived the farewell's time limit"
    );
}
