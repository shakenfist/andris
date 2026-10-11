//! Linking, authentication, sessions and listeners, against an
//! in-process andris on the synthetic platform.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use andris::server::opcodes::{
    CursorOpcode, DisplayOpcode, InputsOpcode, MainOpcode, ServerOpcode,
};
use andris::server::{HandshakeTimeouts, MAX_HANDSHAKES, MAX_HANDSHAKES_PER_PEER};
use shakenfist_spice_protocol::link::SpiceLinkMess;
use shakenfist_spice_protocol::messages::{Disconnecting, MessageHeader, SetAck, WireType};
use shakenfist_spice_protocol::{ChannelType, SpiceError, capabilities, main_client};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use support::{
    TICKET, TestClient, TestServer, WAIT, closed_within, loopback, tcp_from, wait_until,
};

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

/// Short handshake limits, so that tests of them do not wait seconds.
fn short_timeouts() -> HandshakeTimeouts {
    HandshakeTimeouts {
        first_stage: Duration::from_millis(200),
        stage: Duration::from_secs(10),
        wrong_ticket: Duration::from_millis(50),
    }
}

/// How long a test waits to see a refused connection closed. Well under
/// [`WAIT`], but long enough for a busy test machine.
const PROMPTLY: Duration = Duration::from_secs(2);

/// The server must still link a client, and must have released every
/// handshake permit.
async fn still_serves(server: &TestServer) {
    wait_until("every handshake permit is free", || {
        server.handle.handshakes_available() == MAX_HANDSHAKES
    })
    .await;
    let (_main, session) = server.open_main().await;
    assert_ne!(session, 0);
}

#[tokio::test]
async fn stop_waits_for_a_connection_still_in_its_handshake() {
    let mut server = TestServer::start().await;

    // A connection that sends nothing stays in its handshake until the
    // first-stage time limit, seconds away; stop must wait for it.
    let _silent = TcpStream::connect(server.addr()).await.unwrap();
    wait_until("the handshake has started", || {
        server.handle.handshakes_available() == MAX_HANDSHAKES - 1
    })
    .await;
    assert!(
        !server.handle.stop(Duration::from_millis(200)).await,
        "stop returned while a connection was still open"
    );
}

#[tokio::test]
async fn ninth_concurrent_handshake_is_refused() {
    let server = TestServer::start().await;

    // Eight clients, from eight addresses so the per-address cap does not
    // apply, link and then hold their handshakes open, never sending a
    // ticket.
    let mut held = Vec::new();
    for n in 0..MAX_HANDSHAKES as u8 {
        let mut client = TestClient::connect_from(server.addr(), loopback(10 + n)).await;
        assert_eq!(
            client.link(0, ChannelType::Main).await.error,
            SpiceError::Ok
        );
        held.push(client);
    }
    assert_eq!(server.handle.handshakes_available(), 0);

    // A ninth is closed at once, before it sends anything.
    let mut ninth = tcp_from(server.addr(), loopback(30)).await;
    assert!(
        closed_within(&mut ninth, PROMPTLY).await,
        "the ninth connection was not closed promptly"
    );

    // Releasing one handshake frees its permit for a new client.
    drop(held.pop());
    wait_until("a handshake permit is free", || {
        server.handle.handshakes_available() == 1
    })
    .await;
    let mut client = TestClient::connect_from(server.addr(), loopback(31)).await;
    assert_eq!(
        client.link(0, ChannelType::Main).await.error,
        SpiceError::Ok
    );
}

#[tokio::test]
async fn one_address_holds_at_most_four_handshakes() {
    let server = TestServer::start().await;
    let greedy = loopback(40);

    let mut held = Vec::new();
    for _ in 0..MAX_HANDSHAKES_PER_PEER {
        let mut client = TestClient::connect_from(server.addr(), greedy).await;
        assert_eq!(
            client.link(0, ChannelType::Main).await.error,
            SpiceError::Ok
        );
        held.push(client);
    }
    assert_eq!(
        server.handle.handshakes_from(greedy),
        MAX_HANDSHAKES_PER_PEER
    );

    assert_eq!(MAX_HANDSHAKES_PER_PEER, 4);

    // A fifth from the same address is closed at once...
    let mut fifth = tcp_from(server.addr(), greedy).await;
    assert!(
        closed_within(&mut fifth, PROMPTLY).await,
        "a fifth connection from one address was not closed promptly"
    );
    // ...while another address is served, since permits remain.
    let mut other = TestClient::connect_from(server.addr(), loopback(41)).await;
    assert_eq!(other.link(0, ChannelType::Main).await.error, SpiceError::Ok);
    other.auth(TICKET).await.unwrap();

    // Once one of its handshakes ends, the address may start another.
    drop(held.pop());
    wait_until("the address has a free slot", || {
        server.handle.handshakes_from(greedy) == MAX_HANDSHAKES_PER_PEER - 1
    })
    .await;
    let mut client = TestClient::connect_from(server.addr(), greedy).await;
    assert_eq!(
        client.link(0, ChannelType::Main).await.error,
        SpiceError::Ok
    );
}

#[tokio::test]
async fn a_silent_client_is_closed_by_the_first_stage_limit() {
    let server = TestServer::start_with_timeouts(short_timeouts()).await;
    let started = std::time::Instant::now();
    let mut silent = TcpStream::connect(server.addr()).await.unwrap();
    assert!(
        closed_within(&mut silent, PROMPTLY).await,
        "a client that sent nothing was not closed"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "closed before the first stage's limit"
    );
    still_serves(&server).await;
}

#[tokio::test]
async fn a_silent_tls_client_is_closed_by_the_first_stage_limit() {
    let server = TestServer::start_tls_with_timeouts(short_timeouts()).await;
    // TCP only: the TLS handshake never starts.
    let mut silent = TcpStream::connect(server.tls_addr()).await.unwrap();
    assert!(
        closed_within(&mut silent, PROMPTLY).await,
        "a client that sent nothing was not closed"
    );
    wait_until("every handshake permit is free", || {
        server.handle.handshakes_available() == MAX_HANDSHAKES
    })
    .await;
}

#[tokio::test]
async fn a_tls_client_silent_after_the_tls_handshake_is_closed() {
    let server = TestServer::start_tls_with_timeouts(short_timeouts()).await;
    let certificate = server.certificate.clone().unwrap();
    // The TLS handshake completes, then no link message follows.
    let mut silent = TestClient::connect_tls(server.tls_addr(), &certificate).await;
    let started = std::time::Instant::now();
    assert!(
        closed_within(silent.stream_mut(), PROMPTLY).await,
        "a TLS client that sent no link message was not closed"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "closed by a later stage's limit, not the first stage's"
    );
    wait_until("every handshake permit is free", || {
        server.handle.handshakes_available() == MAX_HANDSHAKES
    })
    .await;
}

/// A well-formed main-channel link message, as bytes to tamper with.
fn link_bytes() -> Vec<u8> {
    main_link(capabilities::DEFAULT_COMMON).serialize()
}

/// Send `bytes` raw, optionally half-close, and check the server closes
/// the connection without serving it, then still serves others.
async fn refused_raw(bytes: &[u8], half_close: bool) {
    let server = TestServer::start_with_timeouts(short_timeouts()).await;
    let mut stream = TcpStream::connect(server.addr()).await.unwrap();
    stream.write_all(bytes).await.unwrap();
    if half_close {
        stream.shutdown().await.unwrap();
    }
    assert!(
        closed_within(&mut stream, PROMPTLY).await,
        "the server did not close the connection"
    );
    assert_eq!(server.handle.session_id(), None);
    still_serves(&server).await;
}

#[tokio::test]
async fn bad_magic_is_closed() {
    let mut bytes = link_bytes();
    bytes[..4].copy_from_slice(b"QDER");
    refused_raw(&bytes, false).await;
}

#[tokio::test]
async fn a_wrong_major_version_is_closed() {
    let mut bytes = link_bytes();
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    refused_raw(&bytes, false).await;
}

#[tokio::test]
async fn an_oversized_link_message_is_closed() {
    // Only the header: the declared size is refused before any body.
    let mut bytes = link_bytes();
    bytes[12..16].copy_from_slice(&4097u32.to_le_bytes());
    refused_raw(&bytes[..16], false).await;
}

#[tokio::test]
async fn a_truncated_link_message_is_closed() {
    let bytes = link_bytes();
    refused_raw(&bytes[..bytes.len() - 3], true).await;
}

#[tokio::test]
async fn a_second_link_of_a_type_replaces_the_first() {
    let server = TestServer::start().await;
    let (_main, session) = server.open_main().await;
    let mut first = server.open(session, ChannelType::Display).await;
    expect_set_ack(&mut first).await;
    let mut inputs = server.open(session, ChannelType::Inputs).await;

    let mut second = server.open(session, ChannelType::Display).await;
    expect_set_ack(&mut second).await;
    first.expect_closed().await;

    // The new display channel stays, and nothing else was disturbed.
    assert_eq!(server.session_id(), session);
    assert!(
        !closed_within(second.stream_mut(), Duration::from_millis(200)).await,
        "the new display channel closed"
    );
    assert!(
        !closed_within(inputs.stream_mut(), Duration::from_millis(200)).await,
        "the inputs channel closed"
    );
}

/// One byte over each channel's client message limit closes that channel.
#[tokio::test]
async fn an_oversized_message_closes_each_channel() {
    let cases = [
        (ChannelType::Display, DisplayOpcode::MAX_CLIENT_BODY),
        (ChannelType::Inputs, InputsOpcode::MAX_CLIENT_BODY),
        (ChannelType::Cursor, CursorOpcode::MAX_CLIENT_BODY),
        // Last, because closing the main channel ends the session.
        (ChannelType::Main, MainOpcode::MAX_CLIENT_BODY),
    ];
    let server = TestServer::start().await;
    let (main, session) = server.open_main().await;
    let mut main = Some(main);
    for (channel_type, limit) in cases {
        let mut client = match channel_type {
            ChannelType::Main => main.take().unwrap(),
            _ => server.open(session, channel_type).await,
        };
        let mut message = Vec::new();
        MessageHeader {
            message_type: 60000,
            message_size: u32::try_from(limit + 1).unwrap(),
        }
        .write(&mut message)
        .unwrap();
        message.resize(message.len() + limit + 1, 0);
        // The server may close before the body is all written.
        let _ = client.stream_mut().write_all(&message).await;
        let _ = client.stream_mut().flush().await;
        assert!(
            closed_within(client.stream_mut(), WAIT).await,
            "{channel_type:?} stayed open"
        );
    }
    server.wait_for_no_session().await;
}
