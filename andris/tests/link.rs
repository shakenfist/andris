//! Linking, authentication, sessions and listeners, against an
//! in-process andris on the synthetic platform.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use andris::server::MAX_HANDSHAKES;
use shakenfist_spice_protocol::link::SpiceLinkMess;
use shakenfist_spice_protocol::messages::{Disconnecting, SetAck, WireType};
use shakenfist_spice_protocol::{ChannelType, SpiceError, capabilities, main_client};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use support::{TICKET, TestClient, TestServer, WAIT};

/// The client's `SET_ACK` must be the display and cursor channels' first
/// message, with generation 1 and window 20.
async fn expect_set_ack(client: &mut TestClient) {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, 3, "SET_ACK");
    assert_eq!(
        SetAck::decode(message.payload()).unwrap(),
        SetAck {
            generation: 1,
            window: 20
        }
    );
}

#[tokio::test]
async fn good_ticket_links_every_channel() {
    let server = TestServer::start().await;
    let (_main, session) = server.open_main().await;
    assert_ne!(session, 0);

    let mut display = server.open(session, ChannelType::Display).await;
    expect_set_ack(&mut display).await;
    let mut cursor = server.open(session, ChannelType::Cursor).await;
    expect_set_ack(&mut cursor).await;
    let _inputs = server.open(session, ChannelType::Inputs).await;

    assert_eq!(
        server.session_id(),
        session,
        "secondary links keep the session"
    );
}

#[tokio::test]
async fn wrong_ticket_is_permission_denied() {
    let server = TestServer::start().await;
    let mut client = TestClient::connect(server.addr()).await;
    let reply = client.link(0, ChannelType::Main).await;
    assert_eq!(reply.error, SpiceError::Ok);
    let error = client.auth("not-the-ticket").await.unwrap_err();
    assert!(
        format!("{error:#}").contains("PermissionDenied"),
        "{error:#}"
    );
    client.expect_closed().await;
    assert_eq!(server.handle.session_id(), None);
}

#[tokio::test]
async fn ticket_prefix_is_permission_denied() {
    let server = TestServer::start().await;
    let mut client = TestClient::connect(server.addr()).await;
    client.link(0, ChannelType::Main).await;
    let error = client.auth(&TICKET[..4]).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("PermissionDenied"),
        "{error:#}"
    );
}

/// A main-channel link message with `common` as its common capabilities.
fn main_link(common: u32) -> SpiceLinkMess {
    SpiceLinkMess::new(0, ChannelType::Main, 0, common, 0)
}

#[tokio::test]
async fn client_without_auth_selection_gets_version_mismatch() {
    let server = TestServer::start().await;
    let mut client = TestClient::connect(server.addr()).await;
    let reply = client
        .link_raw(&main_link(
            capabilities::AUTH_SPICE | capabilities::MINI_HEADER,
        ))
        .await;
    assert_eq!(reply.error, SpiceError::VersionMismatch);
    client.expect_closed().await;
}

#[tokio::test]
async fn client_without_mini_header_gets_version_mismatch() {
    let server = TestServer::start().await;
    let mut client = TestClient::connect(server.addr()).await;
    let reply = client
        .link_raw(&main_link(
            capabilities::AUTH_SELECTION | capabilities::AUTH_SPICE,
        ))
        .await;
    assert_eq!(reply.error, SpiceError::VersionMismatch);
}

#[tokio::test]
async fn secondary_channel_with_wrong_connection_id_is_refused() {
    let server = TestServer::start().await;

    // No session at all.
    let mut client = TestClient::connect(server.addr()).await;
    let reply = client.link(1234, ChannelType::Display).await;
    assert_eq!(reply.error, SpiceError::BadConnectionId);
    client.expect_closed().await;

    // A session, but not this one.
    let (_main, session) = server.open_main().await;
    let wrong = session.wrapping_add(1).max(1);
    let mut client = TestClient::connect(server.addr()).await;
    let reply = client.link(wrong, ChannelType::Inputs).await;
    assert_eq!(reply.error, SpiceError::BadConnectionId);
}

#[tokio::test]
async fn unserved_channels_are_unavailable() {
    let server = TestServer::start().await;
    let (_main, session) = server.open_main().await;

    let mut client = TestClient::connect(server.addr()).await;
    let reply = client.link(session, ChannelType::Playback).await;
    assert_eq!(reply.error, SpiceError::ChannelUnavailable);

    let mut client = TestClient::connect(server.addr()).await;
    let link = SpiceLinkMess::new(
        session,
        ChannelType::Display,
        1,
        capabilities::DEFAULT_COMMON,
        0,
    );
    let reply = client.link_raw(&link).await;
    assert_eq!(reply.error, SpiceError::ChannelUnavailable, "channel id 1");
}

#[tokio::test]
async fn second_main_link_closes_first_sessions_channels() {
    let server = TestServer::start().await;
    let (mut main, first) = server.open_main().await;
    let mut display = server.open(first, ChannelType::Display).await;
    expect_set_ack(&mut display).await;
    let mut inputs = server.open(first, ChannelType::Inputs).await;

    let (_main2, second) = server.open_main().await;
    assert_ne!(second, first);

    main.expect_closed().await;
    display.expect_closed().await;
    inputs.expect_closed().await;

    // The old id no longer links; the new one does.
    let mut client = TestClient::connect(server.addr()).await;
    let reply = client.link(first, ChannelType::Cursor).await;
    assert_eq!(reply.error, SpiceError::BadConnectionId);
    let mut cursor = server.open(second, ChannelType::Cursor).await;
    expect_set_ack(&mut cursor).await;
}

#[tokio::test]
async fn closing_the_main_channel_ends_the_session() {
    let server = TestServer::start().await;
    let (main, session) = server.open_main().await;
    let mut inputs = server.open(session, ChannelType::Inputs).await;
    drop(main);
    inputs.expect_closed().await;
    server.wait_for_no_session().await;
}

#[tokio::test]
async fn disconnecting_closes_a_channel() {
    let server = TestServer::start().await;
    let (_main, session) = server.open_main().await;
    let mut inputs = server.open(session, ChannelType::Inputs).await;
    let mut body = Vec::new();
    Disconnecting {
        timestamp: 0,
        reason: 0,
    }
    .write(&mut body);
    inputs.send(main_client::DISCONNECTING, &body).await;
    inputs.expect_closed().await;
    assert_eq!(server.session_id(), session, "only that channel closes");
}

#[tokio::test]
async fn tls_listener_serves_links() {
    let server = TestServer::start_tls().await;
    let certificate = server.certificate.clone().unwrap();
    let mut client = TestClient::connect_tls(server.tls_addr(), &certificate).await;
    let reply = client.link(0, ChannelType::Main).await;
    assert_eq!(reply.error, SpiceError::Ok);
    client.auth(TICKET).await.unwrap();
    let session = server.session_id();

    let mut display = TestClient::connect_tls(server.tls_addr(), &certificate).await;
    assert_eq!(
        display.link(session, ChannelType::Display).await.error,
        SpiceError::Ok
    );
    display.auth(TICKET).await.unwrap();
    expect_set_ack(&mut display).await;
}

#[tokio::test]
async fn plain_listener_redirects_to_tls() {
    let server = TestServer::start_tls().await;
    let mut client = TestClient::connect(server.addr()).await;
    let reply = client.link(0, ChannelType::Main).await;
    assert_eq!(reply.error, SpiceError::NeedSecured);
    client.expect_closed().await;
    assert_eq!(server.handle.session_id(), None);
}

#[tokio::test]
async fn ninth_concurrent_handshake_is_refused() {
    let server = TestServer::start().await;

    // Eight clients link and then hold their handshakes open, never
    // sending a ticket.
    let mut held = Vec::new();
    for _ in 0..MAX_HANDSHAKES {
        let mut client = TestClient::connect(server.addr()).await;
        assert_eq!(
            client.link(0, ChannelType::Main).await.error,
            SpiceError::Ok
        );
        held.push(client);
    }

    // A ninth is closed at once, before it sends anything.
    let mut ninth = TcpStream::connect(server.addr()).await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), ninth.read(&mut byte))
        .await
        .expect("the ninth connection was not closed promptly");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "the ninth connection got data"
    );

    // Releasing one handshake frees its permit for a new client.
    drop(held.pop());
    tokio::time::timeout(WAIT, async {
        loop {
            let mut client = TestClient::connect(server.addr()).await;
            let mut byte = [0u8; 1];
            // A refused connection reads EOF at once; an admitted one
            // waits for a link message.
            let probe = tokio::time::timeout(Duration::from_millis(200), async {
                tokio::io::AsyncReadExt::read(client.stream_mut(), &mut byte).await
            })
            .await;
            if probe.is_err() {
                assert_eq!(
                    client.link(0, ChannelType::Main).await.error,
                    SpiceError::Ok
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no permit was released");
}
