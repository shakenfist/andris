//! The main channel: its opening messages, the channel list, and the
//! mouse mode, against an in-process andris on the synthetic platform.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use andris::server::session::MouseMode;
use shakenfist_spice_protocol::messages::{
    ChannelEntry, ChannelsList, MainInit, MainMouseMode, MouseModeRequest, Ping, ReceivedMessage,
    WireType,
};
use shakenfist_spice_protocol::{
    ChannelType, MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER, main_client, main_server,
};

use support::{TestClient, TestServer};

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
    assert_eq!(init.agent_connected, 0);
    assert_eq!(init.agent_tokens, 0);
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
/// `andris::server::main_channel::CHANNELS`.
#[tokio::test]
async fn attach_channels_lists_display_cursor_and_inputs_last() {
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
    client.send(main_client::AGENT_START, &[0; 4]).await;
    // The channel still answers.
    client.send(main_client::ATTACH_CHANNELS, &[]).await;
    let message = client.recv().await;
    assert_eq!(message.header.message_type, main_server::CHANNELS_LIST);
}
