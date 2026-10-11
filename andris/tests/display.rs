//! The display channel: its start sequence, the draws it sends and how
//! they decode, compression choice, damage-driven frames and their
//! `MARK`s, the rate cap, the ack window, the scrub, resizing and failed
//! captures, against an in-process andris on the synthetic platform.
//!
//! Every draw is decoded, with ryll's `DrawCopy` reader and LZ4 decoder,
//! onto a client-side copy of the surface, which is compared with what
//! the synthetic desktop shows.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::time::{Duration, Instant};

use andris::platform::synthetic::{SQUARE_SIZE, SQUARE_TOP};
use andris::platform::{DisplayGeometry, Frame, Rectangle, Region, Size};
use andris::server::display::{CAPTURE_RETRY, MAX_CAPTURE_FAILURES};
use andris::server::updates::MAX_RECTS_PER_FRAME;
use shakenfist_spice_compression::decompress_spice_lz4;
use shakenfist_spice_protocol::constants::{
    bitmap_flags, bitmap_fmt, clip_type, image_compression, ropd, surface_fmt,
};
use shakenfist_spice_protocol::messages::{
    AckSync, DisplayHead, DisplayInit, DisplayMonitorsConfig, DrawCopy, ImagePayload,
    PreferredCompression, PreferredVideoCodecType, ReceivedMessage, Rect, SetAck, StreamReport,
    SurfaceCreate, SurfaceDestroy, WireType,
};
use shakenfist_spice_protocol::{
    ChannelType, ImageType, capabilities, display_client, display_server,
};

use support::{TestClient, TestServer};

/// The frame interval of the servers here, unless a test says otherwise:
/// short, so tests are quick.
const INTERVAL: Duration = Duration::from_millis(20);

/// How long to wait before concluding the server sends nothing: five
/// frame intervals.
const QUIET: Duration = Duration::from_millis(100);

/// A short scrub period, for the tests that watch the scrub: with a
/// 192-row desktop of three slices, a pass takes 75 ms.
const SHORT_SCRUB: Duration = Duration::from_millis(400);

/// The ack window andris asks for.
const WINDOW: u32 = 20;

fn body<T: WireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write(&mut out);
    out
}

/// What the client knows of the primary surface: its size and RGBA
/// pixels, drawn from the draws it was sent.
struct Screen {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// One decoded DRAW_COPY.
struct Draw {
    bbox: Rect,
    image_type: u8,
    image_id: u64,
    /// The image as RGBA, `width * height` pixels.
    rgba: Vec<u8>,
}

impl Screen {
    fn new(size: Size) -> Self {
        Self {
            width: size.width,
            height: size.height,
            rgba: vec![0; size.width as usize * size.height as usize * 4],
        }
    }

    /// Put `draw`'s image at its box.
    fn apply(&mut self, draw: &Draw) {
        let left = draw.bbox.left as usize;
        let width = (draw.bbox.right - draw.bbox.left) as usize;
        for (row, y) in (draw.bbox.top..draw.bbox.bottom).enumerate() {
            let at = (y as usize * self.width as usize + left) * 4;
            let src = &draw.rgba[row * width * 4..(row + 1) * width * 4];
            self.rgba[at..at + width * 4].copy_from_slice(src);
        }
    }

    /// Assert the screen shows exactly `frame`.
    fn assert_shows(&self, frame: &Frame) {
        assert_eq!((self.width, self.height), (frame.width(), frame.height()));
        for y in 0..frame.height() {
            let row = rgba(frame.row(y).unwrap());
            let at = y as usize * self.width as usize * 4;
            assert!(
                self.rgba[at..at + row.len()] == row[..],
                "row {y} differs from the desktop"
            );
        }
    }
}

/// BGRX to RGBA with opaque alpha, as ryll's LZ4 decoder produces.
fn rgba(bgrx: &[u8]) -> Vec<u8> {
    bgrx.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect()
}

/// Decode a DRAW_COPY, checking every field andris fixes: surface 0, no
/// clip, `OP_PUT`, scale mode 0, no mask, a source area of the whole
/// image, and an image exactly the box's size with no flags.
fn decode_draw(message: &ReceivedMessage) -> Draw {
    assert_eq!(message.header.message_type, display_server::DRAW_COPY);
    let draw = DrawCopy::decode(message.payload()).unwrap();
    assert_eq!(draw.base.surface_id, 0);
    assert_eq!(draw.base.clip.clip_type, clip_type::NONE);
    assert_eq!(draw.rop_descriptor, ropd::OP_PUT);
    assert_eq!(draw.scale_mode, 0);
    assert_eq!(draw.mask.flags, 0);
    assert!(draw.mask.bitmap.is_none());

    let bbox = draw.base.bbox;
    let width = (bbox.right - bbox.left) as u32;
    let height = (bbox.bottom - bbox.top) as u32;
    assert_eq!(
        draw.src_area,
        Rect {
            top: 0,
            left: 0,
            bottom: height as i32,
            right: width as i32,
        }
    );
    let image = draw.src_bitmap.unwrap();
    let descriptor = &image.descriptor;
    assert_eq!(
        (descriptor.width, descriptor.height),
        (width, height),
        "the image descriptor is the box's size"
    );
    assert_eq!(descriptor.flags, 0, "no image is cached");

    let rgba_pixels = match &image.payload {
        ImagePayload::Lz4(data) => {
            assert_eq!(descriptor.image_type, ImageType::Lz4 as u8);
            decompress_spice_lz4(&data.data, width as usize, height as usize)
                .unwrap()
                .pixels
        }
        ImagePayload::Bitmap(bitmap) => {
            assert_eq!(descriptor.image_type, ImageType::Pixmap as u8);
            assert_eq!(bitmap.header.format, bitmap_fmt::BIT32);
            assert_eq!(bitmap.header.flags, bitmap_flags::TOP_DOWN);
            assert_eq!((bitmap.header.x, bitmap.header.y), (width, height));
            assert_eq!(bitmap.header.stride, width * 4);
            rgba(&bitmap.data)
        }
        other => panic!("unexpected image payload {other:?}"),
    };
    Draw {
        bbox,
        image_type: descriptor.image_type,
        image_id: descriptor.image_id,
        rgba: rgba_pixels,
    }
}

/// A server, the main channel that holds its session open, and a display
/// channel linked with `caps`.
async fn open_display(caps: u32) -> (TestServer, TestClient, TestClient) {
    open_display_on(TestServer::start_with_frame_interval(INTERVAL).await, caps).await
}

/// [`open_display`] on `server`.
async fn open_display_on(server: TestServer, caps: u32) -> (TestServer, TestClient, TestClient) {
    let (main, session) = server.open_main().await;
    let display = server
        .open_with_caps(session, ChannelType::Display, caps)
        .await;
    (server, main, display)
}

/// Read `SET_ACK` and check it.
async fn expect_set_ack(client: &mut TestClient) {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::SET_ACK);
    assert_eq!(
        SetAck::decode(message.payload()).unwrap(),
        SetAck {
            generation: 1,
            window: WINDOW
        }
    );
}

/// What ryll sends when its display channel starts: `INIT`, its
/// preferred compression, and its preferred video codecs.
async fn send_init(client: &mut TestClient, compression: u8) {
    let init = DisplayInit {
        cache_id: 1,
        cache_size: 20 * 1024 * 1024,
        glz_dict_id: 0,
        glz_dict_window: 4 * 1024 * 1024,
    };
    client.send(display_client::INIT, &body(&init)).await;
    send_compression(client, compression).await;
    let codecs = PreferredVideoCodecType { codecs: vec![3, 1] };
    client
        .send(display_client::PREFERRED_VIDEO_CODEC_TYPE, &body(&codecs))
        .await;
}

async fn send_compression(client: &mut TestClient, compression: u8) {
    let preferred = PreferredCompression {
        image_compression: compression,
    };
    client
        .send(display_client::PREFERRED_COMPRESSION, &body(&preferred))
        .await;
}

/// Read `SURFACE_CREATE` and check it creates the primary surface at
/// `size`.
async fn expect_surface_create(client: &mut TestClient, size: Size) {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::SURFACE_CREATE);
    assert_eq!(
        SurfaceCreate::decode(message.payload()).unwrap(),
        SurfaceCreate {
            surface_id: 0,
            width: size.width,
            height: size.height,
            format: surface_fmt::FMT_32_XRGB,
            flags: 1,
        }
    );
}

/// `rect` as a DRAW_COPY's box.
fn wire_rect(rect: Rectangle) -> Rect {
    Rect {
        top: rect.y as i32,
        left: rect.x as i32,
        bottom: (rect.y + rect.height) as i32,
        right: (rect.x + rect.width) as i32,
    }
}

/// Read one DRAW_COPY for each of `expected`, in order, onto `screen`.
/// Returns each draw's image type and id.
async fn expect_draws(
    client: &mut TestClient,
    screen: &mut Screen,
    expected: &[Rectangle],
) -> Vec<(u8, u64)> {
    let mut images = Vec::new();
    for &rect in expected {
        let draw = decode_draw(&client.recv().await);
        assert_eq!(draw.bbox, wire_rect(rect));
        screen.apply(&draw);
        images.push((draw.image_type, draw.image_id));
    }
    images
}

/// Read a complete frame onto `screen`: one DRAW_COPY of the whole
/// surface, which at the sizes these tests use is too small to split.
/// Returns its image type and id.
async fn expect_complete_frame(client: &mut TestClient, screen: &mut Screen) -> Vec<(u8, u64)> {
    let size = Size {
        width: screen.width,
        height: screen.height,
    };
    expect_draws(client, screen, &[Rectangle::full(size)]).await
}

/// Read `MONITORS_CONFIG` and check it has one head of `size`.
async fn expect_monitors_config(client: &mut TestClient, size: Size) {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::MONITORS_CONFIG);
    assert_eq!(
        DisplayMonitorsConfig::decode(message.payload()).unwrap(),
        DisplayMonitorsConfig {
            max_allowed: 1,
            heads: vec![DisplayHead {
                monitor_id: 0,
                surface_id: 0,
                width: size.width,
                height: size.height,
                x: 0,
                y: 0,
                flags: 0,
            }],
        }
    );
}

async fn expect_mark(client: &mut TestClient) {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::MARK);
    assert!(message.payload().is_empty());
}

async fn expect_quiet(client: &mut TestClient) {
    expect_quiet_for(client, QUIET).await;
}

async fn expect_quiet_for(client: &mut TestClient, wait: Duration) {
    if let Some(message) = client.recv_within(wait).await {
        panic!(
            "expected nothing, but got opcode {}",
            message.header.message_type
        );
    }
}

/// Read one frame onto `screen`: DRAW_COPYs until a `MARK`. Returns the
/// draws' boxes.
async fn expect_frame(client: &mut TestClient, screen: &mut Screen) -> Vec<Rect> {
    let mut boxes = Vec::new();
    loop {
        let message = client.recv().await;
        match message.header.message_type {
            display_server::DRAW_COPY => {
                let draw = decode_draw(&message);
                screen.apply(&draw);
                boxes.push(draw.bbox);
            }
            display_server::MARK => {
                assert!(message.payload().is_empty());
                assert!(!boxes.is_empty(), "a MARK with no draws before it");
                return boxes;
            }
            other => panic!("expected a DRAW_COPY or MARK, got opcode {other}"),
        }
    }
}

/// Run the whole start sequence with ryll's default capabilities, and
/// return the screen it drew.
async fn start(server: &TestServer, client: &mut TestClient, compression: u8) -> Screen {
    expect_set_ack(client).await;
    send_init(client, compression).await;
    // The size, not a capture, which would take a failure meant for the
    // server.
    let size = server.platform.size().unwrap();
    expect_surface_create(client, size).await;
    let mut screen = Screen::new(size);
    expect_complete_frame(client, &mut screen).await;
    expect_monitors_config(client, size).await;
    expect_mark(client).await;
    screen
}

/// Move the square, and return the rectangles andris should send: the
/// tiles the square left and entered, merged. Every one of those tiles
/// changes, since the square moves by less than a tile.
fn advance(server: &TestServer) -> Vec<Rectangle> {
    let before = server.platform.square_x().unwrap();
    server.platform.advance().unwrap();
    square_tiles(server, before)
}

/// The rectangles a frame sends once the square has moved from `before`
/// to where it is now: the tiles it was in and is in, merged.
fn square_tiles(server: &TestServer, before: u32) -> Vec<Rectangle> {
    let size = server.platform.desktop().unwrap().size();
    let after = server.platform.square_x().unwrap();
    let mut tiles = Region::empty(size);
    for x in [before, after] {
        tiles.add(Rectangle::new(x, SQUARE_TOP, SQUARE_SIZE, SQUARE_SIZE));
    }
    tiles.rectangles(MAX_RECTS_PER_FRAME)
}

/// Move the square, and read the frame of the tiles it crossed onto
/// `screen`: their draws, then a `MARK`. Returns each draw's image type
/// and id.
async fn advance_and_expect_frame(
    server: &TestServer,
    client: &mut TestClient,
    screen: &mut Screen,
) -> Vec<(u8, u64)> {
    let expected = advance(server);
    let images = expect_draws(client, screen, &expected).await;
    expect_mark(client).await;
    screen.assert_shows(&server.platform.desktop().unwrap());
    images
}

#[tokio::test]
async fn start_sequence_waits_for_init_and_runs_in_order() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    expect_set_ack(&mut client).await;
    expect_quiet(&mut client).await;

    send_init(&mut client, image_compression::AUTO_GLZ).await;
    let size = Size {
        width: 256,
        height: 192,
    };
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    let images = expect_complete_frame(&mut client, &mut screen).await;
    let lz4 = ImageType::Lz4 as u8;
    assert_eq!(images, vec![(lz4, 1)]);
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_monitors_config(&mut client, size).await;
    expect_mark(&mut client).await;

    // An unchanging desktop sends nothing more.
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn no_monitors_config_without_the_capability() {
    let caps = capabilities::DEFAULT_DISPLAY & !capabilities::DISPLAY_MONITORS_CONFIG;
    let (server, _main, mut client) = open_display(caps).await;
    expect_set_ack(&mut client).await;
    send_init(&mut client, image_compression::AUTO_GLZ).await;
    let size = server.platform.desktop().unwrap().size();
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    expect_complete_frame(&mut client, &mut screen).await;
    expect_mark(&mut client).await;
}

#[tokio::test]
async fn advance_sends_only_the_tiles_the_square_crossed() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let lz4 = ImageType::Lz4 as u8;
    // Within the first two tile columns, each step is one rectangle of
    // those columns' two tile rows that the square crosses.
    for id in [2, 3, 4] {
        let rects = advance(&server);
        assert_eq!(rects, vec![Rectangle::new(0, 64, 128, 128)]);
        let images = expect_draws(&mut client, &mut screen, &rects).await;
        assert_eq!(images, vec![(lz4, id)]);
        screen.assert_shows(&server.platform.desktop().unwrap());
        // Exactly one MARK closes each frame.
        expect_mark(&mut client).await;
    }
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn a_wrapping_square_sends_two_rectangles() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    // The square's left edge reaches 192, the last place a 64-pixel
    // square fits on a 256-pixel desktop, and then wraps to 0. That is
    // 24 frames of two messages, more than twice the ack window, so the
    // client acknowledges as it goes.
    client
        .send(display_client::ACK_SYNC, &body(&AckSync { generation: 1 }))
        .await;
    while server.platform.square_x().unwrap() < 192 {
        advance_and_expect_frame(&server, &mut client, &mut screen).await;
        // More often than ryll would; the server's count stops at zero.
        client.send(display_client::ACK, &[]).await;
    }
    let rects = advance(&server);
    assert_eq!(
        rects,
        vec![
            Rectangle::new(0, 64, 64, 128),
            Rectangle::new(192, 64, 64, 128),
        ]
    );
    expect_draws(&mut client, &mut screen, &rects).await;
    expect_mark(&mut client).await;
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn bitmaps_for_a_client_without_lz4() {
    let caps = capabilities::DEFAULT_DISPLAY & !capabilities::DISPLAY_LZ4_COMPRESSION;
    let (server, _main, mut client) = open_display(caps).await;
    expect_set_ack(&mut client).await;
    send_init(&mut client, image_compression::LZ4).await;
    let size = server.platform.desktop().unwrap().size();
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    let pixmap = ImageType::Pixmap as u8;
    let images = expect_complete_frame(&mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == pixmap));
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_monitors_config(&mut client, size).await;
    expect_mark(&mut client).await;

    let images = advance_and_expect_frame(&server, &mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == pixmap));
}

#[tokio::test]
async fn preferred_compression_off_is_honoured_whenever_it_arrives() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let pixmap = ImageType::Pixmap as u8;
    let lz4 = ImageType::Lz4 as u8;

    // OFF before INIT: the complete frame is bitmaps. (Ryll sends its
    // preference just after INIT, which may arrive after the server has
    // begun the complete frame; it then applies from the next frame.)
    expect_set_ack(&mut client).await;
    send_compression(&mut client, image_compression::OFF).await;
    send_init(&mut client, image_compression::OFF).await;
    let size = server.platform.desktop().unwrap().size();
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    let images = expect_complete_frame(&mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == pixmap));
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_monitors_config(&mut client, size).await;
    expect_mark(&mut client).await;

    // The server reads a message as it arrives; give it a moment to,
    // before the desktop changes.
    send_compression(&mut client, image_compression::AUTO_GLZ).await;
    tokio::time::sleep(INTERVAL).await;
    let images = advance_and_expect_frame(&server, &mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == lz4));

    send_compression(&mut client, image_compression::OFF).await;
    tokio::time::sleep(INTERVAL).await;
    let images = advance_and_expect_frame(&server, &mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == pixmap));
}

#[tokio::test]
async fn while_acks_are_withheld_damage_merges_into_one_frame() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    // SET_ACK, SURFACE_CREATE, one draw, MONITORS_CONFIG and MARK.
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let mut unacked = 5;

    // A draw and a MARK a frame, until the server has sent one more than
    // twice the window, as many as it may. The square wraps, and sends
    // two draws, only after 24 steps, well beyond this.
    let limit = 2 * WINDOW + 1;
    while unacked < limit {
        let images = advance_and_expect_frame(&server, &mut client, &mut screen).await;
        unacked += images.len() as u32 + 1;
    }
    assert_eq!(unacked, limit);

    // The desktop changes again and again, each change given time for a
    // frame of its own, but the server holds back. Ten steps take the
    // square from 144 across the wrap to 24.
    let before = server.platform.square_x().unwrap();
    for _ in 0..10 {
        server.platform.advance().unwrap();
        tokio::time::sleep(INTERVAL * 2).await;
    }
    expect_quiet(&mut client).await;
    let expected = square_tiles(&server, before);
    assert_eq!(expected, vec![Rectangle::new(0, 64, 256, 128)]);

    // One window acknowledged: one frame follows, of the tiles the square
    // was in when the window filled and the tiles it is in now, and then
    // nothing.
    client
        .send(display_client::ACK_SYNC, &body(&AckSync { generation: 1 }))
        .await;
    client.send(display_client::ACK, &[]).await;
    let boxes = expect_frame(&mut client, &mut screen).await;
    let expected: Vec<Rect> = expected.into_iter().map(wire_rect).collect();
    assert_eq!(boxes, expected);
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn a_size_change_replaces_the_surface() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    assert_eq!(screen.height, 192);

    // 200 rows: the last tile row is 8 rows.
    let size = Size {
        width: 320,
        height: 200,
    };
    server.platform.set_size(size).unwrap();
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::SURFACE_DESTROY);
    assert_eq!(
        SurfaceDestroy::decode(message.payload()).unwrap(),
        SurfaceDestroy { surface_id: 0 }
    );
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    expect_complete_frame(&mut client, &mut screen).await;
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_monitors_config(&mut client, size).await;
    expect_mark(&mut client).await;
    expect_quiet(&mut client).await;

    // Frames carry on at the new size.
    advance_and_expect_frame(&server, &mut client, &mut screen).await;
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn init_with_a_negative_cache_size_closes_the_channel() {
    let (_server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    expect_set_ack(&mut client).await;
    let init = DisplayInit {
        cache_id: 1,
        cache_size: -1,
        glz_dict_id: 0,
        glz_dict_window: 0,
    };
    client.send(display_client::INIT, &body(&init)).await;
    client.expect_closed().await;
}

#[tokio::test]
async fn init_with_a_negative_glz_window_closes_the_channel() {
    let (_server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    expect_set_ack(&mut client).await;
    let init = DisplayInit {
        cache_id: 1,
        cache_size: 0,
        glz_dict_id: 0,
        glz_dict_window: -1,
    };
    client.send(display_client::INIT, &body(&init)).await;
    client.expect_closed().await;
}

#[tokio::test]
async fn stream_reports_codecs_and_unknown_opcodes_are_ignored() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;

    let report = StreamReport {
        stream_id: 1,
        unique_id: 2,
        start_frame_mm_time: 3,
        end_frame_mm_time: 4,
        num_frames: 5,
        num_drops: 6,
        last_frame_delay: -7,
        audio_delay: u32::MAX,
    };
    client
        .send(display_client::STREAM_REPORT, &body(&report))
        .await;
    let codecs = PreferredVideoCodecType { codecs: vec![1] };
    client
        .send(display_client::PREFERRED_VIDEO_CODEC_TYPE, &body(&codecs))
        .await;
    client.send(60000, &[1, 2, 3]).await;

    // The channel still serves frames.
    advance_and_expect_frame(&server, &mut client, &mut screen).await;
}

#[tokio::test]
async fn a_malformed_stream_report_closes_the_channel() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    client.send(display_client::STREAM_REPORT, &[0; 4]).await;
    client.expect_closed().await;
}

#[tokio::test]
async fn failed_captures_skip_frames_and_the_count_resets() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;

    // One short of the limit, twice: the second run would pass the limit
    // if a successful capture did not reset the count.
    for _ in 0..2 {
        server
            .platform
            .fail_captures(MAX_CAPTURE_FAILURES - 1)
            .unwrap();
        advance_and_expect_frame(&server, &mut client, &mut screen).await;
    }
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn a_failed_first_capture_delays_the_surface() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    server.platform.fail_captures(3).unwrap();
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    advance_and_expect_frame(&server, &mut client, &mut screen).await;
}

#[tokio::test]
async fn too_many_failed_captures_in_a_row_close_the_channel() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    server.platform.fail_captures(MAX_CAPTURE_FAILURES).unwrap();
    // Damage, so that the first failure comes at once, from a frame,
    // rather than at the scrub's next tick.
    let failing = Instant::now();
    server.platform.advance().unwrap();
    client.expect_closed().await;
    // Every failure after the first waits out the retry gap, however
    // short the frame interval, whether a frame or the scrub retries.
    let took = failing.elapsed();
    let least = CAPTURE_RETRY * (MAX_CAPTURE_FAILURES - 1);
    assert!(
        took >= least,
        "{MAX_CAPTURE_FAILURES} failures closed the channel after {took:?}, under {least:?}"
    );
}

#[tokio::test]
async fn a_failed_scrub_capture_holds_off_the_next_frame() {
    // The scrub ticks every 25 ms, so it meets the failure before a frame
    // does.
    let server = TestServer::start_with_intervals(INTERVAL, SHORT_SCRUB).await;
    let (server, _main, mut client) = open_display_on(server, capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    server.platform.fail_captures(1).unwrap();
    tokio::time::sleep(SHORT_SCRUB / 4).await;
    // The failure is used up; the frame for this change still waits out
    // the gap the scrub's failure began.
    let changed = Instant::now();
    let rects = advance(&server);
    expect_draws(&mut client, &mut screen, &rects).await;
    expect_mark(&mut client).await;
    let took = changed.elapsed();
    assert!(
        took >= CAPTURE_RETRY - SHORT_SCRUB / 4 - Duration::from_millis(30),
        "the frame came {took:?} after the change"
    );
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn an_idle_desktop_sends_nothing_after_the_first_frame() {
    // A short scrub, so that it reads the whole desktop a dozen times.
    let server = TestServer::start_with_intervals(INTERVAL, SHORT_SCRUB).await;
    let (server, _main, mut client) = open_display_on(server, capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    expect_quiet_for(&mut client, Duration::from_secs(1)).await;
}

#[tokio::test]
async fn damage_that_changed_nothing_sends_nothing_not_even_a_mark() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let size = server.platform.desktop().unwrap().size();
    server
        .platform
        .report_damage(Rectangle::new(10, 10, 100, 100))
        .unwrap();
    expect_quiet(&mut client).await;
    server
        .platform
        .report_damage(Rectangle::full(size))
        .unwrap();
    expect_quiet(&mut client).await;

    // A real change still arrives, and only it.
    advance_and_expect_frame(&server, &mut client, &mut screen).await;
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn a_change_damage_missed_arrives_through_the_scrub() {
    let server = TestServer::start_with_intervals(INTERVAL, SHORT_SCRUB).await;
    let (server, _main, mut client) = open_display_on(server, capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;

    // In the bottom right tile, which is 64 x 64, far from the square.
    server
        .platform
        .paint_undamaged(Rectangle::new(200, 140, 20, 20), (1, 2, 3))
        .unwrap();
    let painted = Instant::now();
    let boxes = expect_frame(&mut client, &mut screen).await;
    let took = painted.elapsed();
    assert!(
        took < SHORT_SCRUB + Duration::from_millis(500),
        "the change took {took:?} to arrive"
    );
    assert_eq!(boxes, vec![wire_rect(Rectangle::new(192, 128, 64, 64))]);
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn the_rate_cap_holds_under_continuous_damage() {
    let interval = Duration::from_millis(100);
    let server = TestServer::start_with_frame_interval(interval).await;
    let (server, _main, mut client) = open_display_on(server, capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;

    // Acknowledge as ryll does, a window at a time, so that the window
    // never limits the frame rate. Five messages have arrived.
    client
        .send(display_client::ACK_SYNC, &body(&AckSync { generation: 1 }))
        .await;
    let mut received = 5;

    // The square moves every 10 ms for a second.
    let platform = server.platform.clone();
    let mover = tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_millis(10));
        for _ in 0..100 {
            ticks.tick().await;
            platform.advance().unwrap();
        }
    });

    // Read until the frames stop.
    let mut marks = 0;
    while let Some(message) = client.recv_within(Duration::from_millis(500)).await {
        received += 1;
        if received % WINDOW == 0 {
            client.send(display_client::ACK, &[]).await;
        }
        match message.header.message_type {
            display_server::DRAW_COPY => screen.apply(&decode_draw(&message)),
            display_server::MARK => marks += 1,
            other => panic!("expected a DRAW_COPY or MARK, got opcode {other}"),
        }
    }
    mover.await.unwrap();

    // A second at one frame per 100 ms is at most eleven frames, and one
    // more for damage after the last of them.
    assert!(marks <= 12, "{marks} frames in a second");
    assert!(marks >= 5, "only {marks} frames in a second");
    screen.assert_shows(&server.platform.desktop().unwrap());
}
