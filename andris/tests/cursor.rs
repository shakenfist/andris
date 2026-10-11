//! The cursor channel: `SET_ACK` and `CURSOR_INIT`, a `CURSOR_SET` for
//! each shape change, the size limit, and the ack window, against an
//! in-process andris on the synthetic platform.
//!
//! Each cursor is read with ryll's `CursorInit` and `CursorSet` and
//! turned back into a `CursorShape`, which must equal the shape the
//! synthetic platform was given.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use andris::platform::CursorShape;
use andris::platform::synthetic::default_cursor;
use shakenfist_spice_protocol::constants::{cursor_flags, cursor_type};
use shakenfist_spice_protocol::messages::{
    AckSync, CursorInit, CursorInitHead, CursorSet, CursorSetHead, SetAck, SpiceCursor, WireType,
};
use shakenfist_spice_protocol::{ChannelType, cursor_client, cursor_server};

use support::{TestClient, TestServer};

/// How long to wait before concluding the server sends nothing.
const QUIET: Duration = Duration::from_millis(100);

/// The ack window andris asks for.
const WINDOW: u32 = 20;

fn body<T: WireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write(&mut out);
    out
}

/// The shape a `SpiceCursor` carries, checking it is an `ALPHA` cursor
/// with `unique` 0 and no flags; `None` for no cursor.
fn shape_of(cursor: &SpiceCursor) -> Option<CursorShape> {
    let Some(header) = &cursor.header else {
        assert_eq!(cursor.flags, cursor_flags::NONE);
        assert!(cursor.data.is_empty());
        return None;
    };
    assert_eq!(cursor.flags, 0, "no cache flags");
    assert_eq!(header.unique_id, 0);
    assert_eq!(header.cursor_type, cursor_type::ALPHA);
    let argb = cursor
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| u32::from_le_bytes(*bytes))
        .collect();
    Some(
        CursorShape::new(
            header.width,
            header.height,
            header.hot_spot_x,
            header.hot_spot_y,
            argb,
        )
        .unwrap(),
    )
}

/// A shape of `width` x `height`, every pixel `colour`, hot spot (0, 0).
fn solid(width: u16, height: u16, colour: u32) -> CursorShape {
    CursorShape::new(
        width,
        height,
        0,
        0,
        vec![colour; width as usize * height as usize],
    )
    .unwrap()
}

/// The main channel that holds the session open, and a cursor channel
/// whose `SET_ACK` has been read and checked.
async fn open_cursor(server: &TestServer) -> (TestClient, TestClient) {
    let (main, session) = server.open_main().await;
    let mut cursor = server.open(session, ChannelType::Cursor).await;
    let message = cursor.recv().await;
    assert_eq!(message.header.message_type, cursor_server::SET_ACK);
    assert_eq!(
        SetAck::decode(message.payload()).unwrap(),
        SetAck {
            generation: 1,
            window: WINDOW
        }
    );
    (main, cursor)
}

/// Read `CURSOR_INIT`, check its fixed fields, and return its shape.
async fn expect_init(client: &mut TestClient, visible: bool) -> Option<CursorShape> {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, cursor_server::INIT);
    let init = CursorInit::decode(message.payload()).unwrap();
    assert_eq!(
        init.head,
        CursorInitHead {
            x: 0,
            y: 0,
            trail_length: 0,
            trail_frequency: 0,
            visible: u8::from(visible),
        }
    );
    shape_of(&init.cursor)
}

/// Read `CURSOR_SET`, check its fixed fields, and return its shape.
async fn expect_set(client: &mut TestClient, visible: bool) -> Option<CursorShape> {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, cursor_server::SET);
    let set = CursorSet::decode(message.payload()).unwrap();
    assert_eq!(
        set.head,
        CursorSetHead {
            x: 0,
            y: 0,
            visible: u8::from(visible),
        }
    );
    shape_of(&set.cursor)
}

async fn expect_quiet(client: &mut TestClient) {
    if let Some(message) = client.recv_within(QUIET).await {
        panic!(
            "expected nothing, but got opcode {}",
            message.header.message_type
        );
    }
}

#[tokio::test]
async fn init_precedes_set_and_both_round_trip_the_shape() {
    let server = TestServer::start().await;
    let (_main, mut cursor) = open_cursor(&server).await;
    assert_eq!(expect_init(&mut cursor, true).await, Some(default_cursor()));
    expect_quiet(&mut cursor).await;

    // Pixels whose four bytes all differ, so a byte-order slip shows.
    let shape = CursorShape::new(
        3,
        2,
        2,
        1,
        vec![
            0x8011_2233,
            0xFF44_5566,
            0x0077_8899,
            0x10AA_BBCC,
            0xFFDD_EEFF,
            0x0102_0304,
        ],
    )
    .unwrap();
    server.platform.set_cursor(Some(shape.clone()));
    assert_eq!(expect_set(&mut cursor, true).await, Some(shape));
    expect_quiet(&mut cursor).await;
}

#[tokio::test]
async fn no_cursor_is_none_and_not_visible() {
    let server = TestServer::start().await;
    server.platform.set_cursor(None);
    let (_main, mut cursor) = open_cursor(&server).await;
    assert_eq!(expect_init(&mut cursor, false).await, None);

    let dot = solid(1, 1, 0xFF00_0000);
    server.platform.set_cursor(Some(dot.clone()));
    assert_eq!(expect_set(&mut cursor, true).await, Some(dot));
    server.platform.set_cursor(None);
    assert_eq!(expect_set(&mut cursor, false).await, None);
}

#[tokio::test]
async fn shapes_over_256_pixels_a_side_are_dropped() {
    let server = TestServer::start().await;
    let (_main, mut cursor) = open_cursor(&server).await;
    expect_init(&mut cursor, true).await;

    server.platform.set_cursor(Some(solid(257, 1, 0xFFFF_FFFF)));
    expect_quiet(&mut cursor).await;
    server.platform.set_cursor(Some(solid(1, 257, 0xFFFF_FFFF)));
    expect_quiet(&mut cursor).await;

    // The largest allowed is sent whole.
    let largest = solid(256, 256, 0x80FF_0000);
    server.platform.set_cursor(Some(largest.clone()));
    assert_eq!(expect_set(&mut cursor, true).await, Some(largest));
}

#[tokio::test]
async fn an_oversized_initial_shape_is_sent_as_none() {
    let server = TestServer::start().await;
    server
        .platform
        .set_cursor(Some(solid(300, 300, 0xFFFF_FFFF)));
    let (_main, mut cursor) = open_cursor(&server).await;
    assert_eq!(expect_init(&mut cursor, true).await, None);
}

#[tokio::test]
async fn nothing_is_sent_beyond_twice_the_ack_window_until_acks_arrive() {
    let server = TestServer::start().await;
    let (_main, mut cursor) = open_cursor(&server).await;
    expect_init(&mut cursor, true).await;
    // SET_ACK and INIT.
    let mut unacked = 2;

    let limit = 2 * WINDOW + 1;
    let mut colour = 0xFF00_0000;
    while unacked < limit {
        colour += 1;
        let shape = solid(2, 2, colour);
        server.platform.set_cursor(Some(shape.clone()));
        assert_eq!(expect_set(&mut cursor, true).await, Some(shape));
        unacked += 1;
    }

    // Two more changes while the window is full: the server holds back.
    server.platform.set_cursor(Some(solid(2, 2, 0xFF12_3456)));
    expect_quiet(&mut cursor).await;
    let latest = solid(4, 4, 0xFF65_4321);
    server.platform.set_cursor(Some(latest.clone()));
    expect_quiet(&mut cursor).await;

    // One window acknowledged: only the latest shape follows.
    cursor
        .send(cursor_client::ACK_SYNC, &body(&AckSync { generation: 1 }))
        .await;
    cursor.send(cursor_client::ACK, &[]).await;
    assert_eq!(expect_set(&mut cursor, true).await, Some(latest));
    expect_quiet(&mut cursor).await;
}
