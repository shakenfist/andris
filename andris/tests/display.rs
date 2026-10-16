//! The display channel: its start sequence, the draws it sends and how
//! they decode, compression choice, damage-driven frames and their
//! `MARK`s, the rate cap, the ack window, the scrub, resizing, failed
//! captures, JPEG with its lossless refresh, and video streams, against
//! an in-process andris on the synthetic platform.
//!
//! Every draw is decoded, with ryll's `DrawCopy` reader and its LZ4 and
//! JPEG decoders, onto a client-side copy of the surface, which is
//! compared with what the synthetic desktop shows. Stream frames are
//! decoded with ryll's H.264 stream decoder, and compared with the
//! desktop within a tolerance.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use andris::platform::synthetic::{SQUARE_SIZE, SQUARE_TOP};
use andris::platform::{DisplayGeometry, Frame, Rectangle, Region, Size};
use andris::server::display::{CAPTURE_RETRY, DisplayOptions, MAX_CAPTURE_FAILURES};
use andris::server::updates::{
    JpegQuality, MAX_RECTS_PER_FRAME, STREAM_LATENCY_BUDGET, STREAM_QUIET,
};
use shakenfist_spice_compression::{
    JpegDecoder, JpegDecoderRsDecoder, VideoDecoder, decompress_spice_lz4, for_stream,
};
use shakenfist_spice_protocol::constants::{
    bitmap_flags, bitmap_fmt, clip_type, image_compression, ropd, stream_flags, surface_fmt,
};
use shakenfist_spice_protocol::messages::{
    AckSync, Clip, DisplayHead, DisplayInit, DisplayMonitorsConfig, DrawCopy, ImagePayload,
    MainInit, PreferredCompression, PreferredVideoCodecType, ReceivedMessage, Rect, SetAck,
    StreamActivateReport, StreamCreate, StreamData, StreamDestroy, StreamReport, SurfaceCreate,
    SurfaceDestroy, WireType,
};
use shakenfist_spice_protocol::{
    ChannelType, ImageType, capabilities, display_client, display_server, main_server,
};
use tracing_subscriber::fmt::MakeWriter;

use support::{TestClient, TestServer, display_options, wait_until};

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
        ImagePayload::Jpeg(data) => {
            assert_eq!(descriptor.image_type, ImageType::Jpeg as u8);
            assert!(data.data.starts_with(&[0xFF, 0xD8]), "no SOI marker");
            let decoded = JpegDecoderRsDecoder::new().decode(&data.data).unwrap();
            assert_eq!(
                (decoded.width, decoded.height),
                (width, height),
                "the JPEG is the box's size"
            );
            decoded.rgba
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
    expect_frame_images(client, screen)
        .await
        .into_iter()
        .map(|(bbox, _)| bbox)
        .collect()
}

/// [`expect_frame`], returning each draw's box and image type.
async fn expect_frame_images(client: &mut TestClient, screen: &mut Screen) -> Vec<(Rect, u8)> {
    let mut draws = Vec::new();
    loop {
        let message = client.recv().await;
        match message.header.message_type {
            display_server::DRAW_COPY => {
                let draw = decode_draw(&message);
                screen.apply(&draw);
                draws.push((draw.bbox, draw.image_type));
            }
            display_server::MARK => {
                assert!(message.payload().is_empty());
                assert!(!draws.is_empty(), "a MARK with no draws before it");
                return draws;
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

/// Expect the surface to be replaced by one of `size`, and the new
/// surface's first frame to show the desktop exactly.
async fn expect_replaced_surface(
    server: &TestServer,
    client: &mut TestClient,
    size: Size,
) -> Screen {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::SURFACE_DESTROY);
    assert_eq!(
        SurfaceDestroy::decode(message.payload()).unwrap(),
        SurfaceDestroy { surface_id: 0 }
    );
    expect_surface_create(client, size).await;
    let mut screen = Screen::new(size);
    expect_complete_frame(client, &mut screen).await;
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_monitors_config(client, size).await;
    expect_mark(client).await;
    screen
}

#[tokio::test]
async fn a_frame_that_finds_an_unreported_size_replaces_the_surface() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;

    // The capture thread sees the new size before damage reports it: the
    // damage is still at the old size, inside the surface.
    let size = Size {
        width: 320,
        height: 256,
    };
    server.platform.set_size_unreported(size).unwrap();
    server
        .platform
        .report_damage(Rectangle::new(0, 0, 64, 64))
        .unwrap();
    let mut screen = expect_replaced_surface(&server, &mut client, size).await;
    expect_quiet(&mut client).await;

    // The resize, reported late, finds nothing left to send.
    server.platform.set_size(size).unwrap();
    expect_quiet(&mut client).await;
    advance_and_expect_frame(&server, &mut client, &mut screen).await;
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn a_scrub_that_finds_an_unreported_size_replaces_the_surface() {
    let server = TestServer::start_with_intervals(INTERVAL, SHORT_SCRUB).await;
    let (server, _main, mut client) = open_display_on(server, capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;

    // No damage at all: only the scrub's capture can notice.
    let size = Size {
        width: 192,
        height: 192,
    };
    server.platform.set_size_unreported(size).unwrap();
    let resized = Instant::now();
    let mut screen = expect_replaced_surface(&server, &mut client, size).await;
    let took = resized.elapsed();
    assert!(
        took < SHORT_SCRUB + Duration::from_millis(500),
        "the resize took {took:?} to arrive"
    );
    expect_quiet(&mut client).await;

    server.platform.set_size(size).unwrap();
    expect_quiet(&mut client).await;
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
async fn a_malformed_preferred_compression_closes_the_channel() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    // The body is one byte, the compression; none is malformed.
    client
        .send(display_client::PREFERRED_COMPRESSION, &[])
        .await;
    client.expect_closed().await;
}

#[tokio::test]
async fn a_malformed_preferred_video_codec_type_closes_the_channel() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    // Three codecs claimed, one sent.
    client
        .send(display_client::PREFERRED_VIDEO_CODEC_TYPE, &[3, 1])
        .await;
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
    // more for damage after the last of them. The upper bound is the
    // rate cap, and holds however slow the machine. The lower bound only
    // shows that frames kept flowing, so it is loose: a loaded CI runner
    // may run the mover, or the server, well behind time.
    assert!(marks <= 12, "{marks} frames in a second");
    assert!(marks >= 3, "only {marks} frames in a second");
    screen.assert_shows(&server.platform.desktop().unwrap());
}

/// The lossless delay of the servers the JPEG tests start: short, so the
/// tests are quick, but long against the frame interval.
const LOSSLESS: Duration = Duration::from_millis(200);

/// Where the JPEG tests paint a photograph: four tiles, away from the
/// square, which one rectangle covers.
const PHOTO: Rectangle = Rectangle::new(128, 0, 128, 128);

/// Whether `image_type` is lossless: LZ4 or a bitmap. Whether LZ4 beats
/// a bitmap on the synthetic photograph, whose grain all but defeats it,
/// depends on the rectangle.
fn is_lossless(image_type: u8) -> bool {
    image_type == ImageType::Lz4 as u8 || image_type == ImageType::Pixmap as u8
}

/// A server whose lossy areas are refreshed after [`LOSSLESS`], sending
/// JPEG at `jpeg_quality`; its main channel; and a display channel with
/// ryll's default capabilities, through its start sequence with
/// `compression` as the client's preference.
async fn start_jpeg(
    jpeg_quality: Option<JpegQuality>,
    compression: u8,
) -> (TestServer, TestClient, TestClient, Screen) {
    let server = TestServer::start_tuned(DisplayOptions {
        frame_interval: INTERVAL,
        lossless_delay: LOSSLESS,
        jpeg_quality,
        ..display_options()
    })
    .await;
    let (server, main, mut client) = open_display_on(server, capabilities::DEFAULT_DISPLAY).await;
    let screen = start(&server, &mut client, compression).await;
    (server, main, client, screen)
}

/// Whether `screen` shows exactly `frame`.
fn shows(screen: &Screen, frame: &Frame) -> bool {
    (0..frame.height()).all(|y| {
        let row = rgba(frame.row(y).unwrap());
        let at = y as usize * screen.width as usize * 4;
        screen.rgba[at..at + row.len()] == row[..]
    })
}

/// Paint [`PHOTO`] and read the frame that sends it, which must be one
/// JPEG of it. Returns when the frame's `MARK` arrived.
async fn expect_photo_as_jpeg(
    server: &TestServer,
    client: &mut TestClient,
    screen: &mut Screen,
) -> Instant {
    server.platform.paint_photo(PHOTO).unwrap();
    let draws = expect_frame_images(client, screen).await;
    let sent = Instant::now();
    assert_eq!(draws, vec![(wire_rect(PHOTO), ImageType::Jpeg as u8)]);
    let desktop = server.platform.desktop().unwrap();
    assert!(!shows(screen, &desktop), "a JPEG of a photo is not exact");
    sent
}

/// Read a lossless refresh of `rects` of [`PHOTO`]: their draws, and a
/// `MARK`.
async fn expect_photo_refresh(client: &mut TestClient, screen: &mut Screen, rects: &[Rectangle]) {
    let draws = expect_frame_images(client, screen).await;
    let boxes: Vec<Rect> = draws.iter().map(|&(bbox, _)| bbox).collect();
    let expected: Vec<Rect> = rects.iter().map(|&rect| wire_rect(rect)).collect();
    assert_eq!(boxes, expected);
    for (bbox, image_type) in draws {
        assert!(
            is_lossless(image_type),
            "{bbox:?} is image type {image_type}"
        );
    }
}

#[tokio::test]
async fn a_photograph_arrives_as_jpeg_and_then_exactly() {
    for compression in [image_compression::AUTO_GLZ, image_compression::AUTO_LZ] {
        let (server, _main, mut client, mut screen) =
            start_jpeg(Some(JpegQuality::DEFAULT), compression).await;
        let sent = expect_photo_as_jpeg(&server, &mut client, &mut screen).await;

        // Nothing until the photograph has been still for LOSSLESS, and
        // then a frame of its own sends it again losslessly.
        expect_quiet_for(&mut client, LOSSLESS - Duration::from_millis(50)).await;
        expect_photo_refresh(&mut client, &mut screen, &[PHOTO]).await;
        screen.assert_shows(&server.platform.desktop().unwrap());
        // The server's clock started as it sent the JPEG, a little before
        // the client's.
        let took = sent.elapsed();
        assert!(
            took >= LOSSLESS - Duration::from_millis(30),
            "the refresh came after {took:?}"
        );

        // Once exact, nothing more.
        expect_quiet_for(&mut client, LOSSLESS * 2).await;
    }
}

/// Report damage that changes nothing on `area` every quarter of
/// [`LOSSLESS`] for `spell`, asserting that nothing arrives meanwhile.
/// Returns when the damage stopped.
async fn damage_quietly(
    server: &TestServer,
    client: &mut TestClient,
    area: Rectangle,
    spell: Duration,
) -> Instant {
    let damaging = Instant::now();
    while damaging.elapsed() < spell {
        server.platform.report_damage(area).unwrap();
        expect_quiet_for(client, LOSSLESS / 4).await;
    }
    Instant::now()
}

#[tokio::test]
async fn damage_on_a_lossy_area_postpones_its_refresh() {
    let (server, _main, mut client, mut screen) =
        start_jpeg(Some(JpegQuality::DEFAULT), image_compression::AUTO_GLZ).await;
    expect_photo_as_jpeg(&server, &mut client, &mut screen).await;

    // Damage that changes nothing, on all of the photograph, for three
    // times LOSSLESS: each postpones the refresh, and sends nothing.
    let stopped = damage_quietly(&server, &mut client, PHOTO, LOSSLESS * 3).await;
    expect_photo_refresh(&mut client, &mut screen, &[PHOTO]).await;
    let took = stopped.elapsed();
    assert!(
        took >= LOSSLESS - LOSSLESS / 4,
        "the refresh came {took:?} after the damage stopped"
    );
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_quiet_for(&mut client, LOSSLESS * 2).await;
}

#[tokio::test]
async fn a_still_lossy_tile_is_refreshed_while_another_is_damaged() {
    let (server, _main, mut client, mut screen) =
        start_jpeg(Some(JpegQuality::DEFAULT), image_compression::AUTO_GLZ).await;
    expect_photo_as_jpeg(&server, &mut client, &mut screen).await;

    // Damage on the photograph's bottom right tile alone: the other three
    // are refreshed once still for LOSSLESS, while it is damaged still.
    let damaged = Rectangle::new(192, 64, 64, 64);
    let platform = server.platform.clone();
    let damager = tokio::spawn(async move {
        let start = Instant::now();
        while start.elapsed() < LOSSLESS * 3 {
            platform.report_damage(damaged).unwrap();
            tokio::time::sleep(LOSSLESS / 4).await;
        }
    });
    let still = [
        Rectangle::new(128, 0, 128, 64),
        Rectangle::new(128, 64, 64, 64),
    ];
    expect_photo_refresh(&mut client, &mut screen, &still).await;
    assert!(!damager.is_finished(), "the refresh waited for the damage");
    damager.await.unwrap();

    // Then the damaged tile, once it too has been still for LOSSLESS.
    expect_photo_refresh(&mut client, &mut screen, &[damaged]).await;
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_quiet_for(&mut client, LOSSLESS * 2).await;
}

#[tokio::test]
async fn a_photograph_is_lossless_unless_the_server_may_choose_and_jpeg_is_on() {
    for (quality, compression) in [
        (Some(JpegQuality::DEFAULT), image_compression::LZ4),
        (Some(JpegQuality::DEFAULT), image_compression::GLZ),
        (Some(JpegQuality::DEFAULT), image_compression::OFF),
        (None, image_compression::AUTO_GLZ),
    ] {
        let (server, _main, mut client, mut screen) = start_jpeg(quality, compression).await;
        server.platform.paint_photo(PHOTO).unwrap();
        let draws = expect_frame_images(&mut client, &mut screen).await;
        let what = format!("quality {quality:?}, compression {compression}");
        assert_eq!(draws.len(), 1, "{what}");
        let (bbox, image_type) = draws[0];
        assert_eq!(bbox, wire_rect(PHOTO), "{what}");
        if compression == image_compression::OFF {
            assert_eq!(image_type, ImageType::Pixmap as u8, "{what}");
        } else {
            assert!(is_lossless(image_type), "{what}: image type {image_type}");
        }
        screen.assert_shows(&server.platform.desktop().unwrap());
        // Nothing was lossy, so nothing is refreshed.
        expect_quiet_for(&mut client, LOSSLESS * 2).await;
    }
}

#[tokio::test]
async fn a_lossy_area_sent_again_losslessly_is_not_refreshed() {
    let (server, _main, mut client, mut screen) =
        start_jpeg(Some(JpegQuality::DEFAULT), image_compression::AUTO_GLZ).await;
    expect_photo_as_jpeg(&server, &mut client, &mut screen).await;

    // The photograph turns into something flat, which goes losslessly.
    server.platform.paint(PHOTO, (10, 20, 30)).unwrap();
    let draws = expect_frame_images(&mut client, &mut screen).await;
    assert_eq!(draws, vec![(wire_rect(PHOTO), ImageType::Lz4 as u8)]);
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_quiet_for(&mut client, LOSSLESS * 2).await;
}

#[tokio::test]
async fn a_resize_forgets_the_lossy_areas() {
    let (server, _main, mut client, mut screen) =
        start_jpeg(Some(JpegQuality::DEFAULT), image_compression::AUTO_GLZ).await;
    expect_photo_as_jpeg(&server, &mut client, &mut screen).await;

    let size = Size {
        width: 320,
        height: 200,
    };
    server.platform.set_size(size).unwrap();
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::SURFACE_DESTROY);
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    // The new surface's complete frame is not photographic as a whole, so
    // it goes losslessly, and there is nothing lossy left to refresh.
    let images = expect_complete_frame(&mut client, &mut screen).await;
    assert_eq!(images[0].0, ImageType::Lz4 as u8);
    screen.assert_shows(&server.platform.desktop().unwrap());
    expect_monitors_config(&mut client, size).await;
    expect_mark(&mut client).await;
    expect_quiet_for(&mut client, LOSSLESS * 2).await;
}

#[tokio::test]
async fn the_counters_move_as_frames_are_sent() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let session = server.handle.session().unwrap();

    // The start sequence: one frame, its draws, and what it took.
    let first = session.stats();
    assert_eq!(first.frames, 1);
    assert_eq!(first.refreshes, 0);
    assert!(first.draws() > 0);
    assert_eq!(first.lz4_draws, first.draws());
    assert!(first.lz4_bytes > 0);
    assert!(first.display_bytes > first.lz4_bytes);
    assert!(first.capture.count > 0);
    assert!(first.encode.count > 0);
    assert!(first.captured_area > 0);
    assert_eq!(first.changed_area, first.captured_area);

    advance_and_expect_frame(&server, &mut client, &mut screen).await;
    let second = session.stats();
    assert_eq!(second.frames, 2);
    assert!(second.draws() > first.draws());
    assert!(second.display_bytes > first.display_bytes);
    assert!(second.capture.count > first.capture.count);
    assert!(second.encode.count > first.encode.count);
    assert!(second.changed_area > first.changed_area);
    assert!(second.changed_area - first.changed_area <= second.captured_area - first.captured_area);
    let delta = second.since(&first);
    assert!(delta.has_frames());
    assert!(delta.summary(Duration::from_secs(1)).contains("1.0 fps"));
}

#[tokio::test]
async fn ack_lag_is_recorded_once_a_window_is_acknowledged() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let session = server.handle.session().unwrap();
    let mut sent = 5;
    while sent < WINDOW {
        let images = advance_and_expect_frame(&server, &mut client, &mut screen).await;
        sent += images.len() as u32 + 1;
    }
    assert_eq!(session.stats().ack_lag.count, 0, "nothing acked yet");

    client
        .send(display_client::ACK_SYNC, &body(&AckSync { generation: 1 }))
        .await;
    client.send(display_client::ACK, &[]).await;
    wait_until("the ack lag is recorded", || {
        session.stats().ack_lag.count == 1
    })
    .await;
    assert!(session.stats().ack_lag.latest().is_some());
}

/// A desktop with room for a video below the square's path, and for a
/// second beside it.
const WIDE: Size = Size {
    width: 640,
    height: 384,
};

/// Where the stream tests play a video: tile-aligned, below the rows the
/// square crosses.
const VIDEO: Rectangle = Rectangle::new(320, 192, 256, 192);

/// Where a video moves to: a tile column apart from [`VIDEO`].
const MOVED: Rectangle = Rectangle::new(0, 192, 256, 192);

/// Every row the square crosses on a [`WIDE`] desktop.
const SQUARE_PATH: Rectangle = Rectangle::new(0, SQUARE_TOP, WIDE.width, SQUARE_SIZE);

/// The most a decoded stream frame may differ from the desktop, as a mean
/// over its pixels' red, green and blue. A frame of the video measured
/// about 3 from the desktop it showed, and about 8 from the desktop a
/// step of the video earlier, so this tells a current frame from a stale
/// one.
const STREAM_TOLERANCE: f64 = 5.0;

/// A server with a [`WIDE`] desktop and the frame interval of the servers
/// here, its main channel, and a display channel linked with `caps`.
async fn open_wide(caps: u32) -> (TestServer, TestClient, TestClient) {
    let display = DisplayOptions {
        frame_interval: INTERVAL,
        ..display_options()
    };
    open_display_on(TestServer::start_sized(WIDE, display).await, caps).await
}

/// Changes the desktop every [`INTERVAL`], on a task of its own, until
/// dropped.
struct Ticker(tokio::task::JoinHandle<()>);

impl Ticker {
    fn start(mut step: impl FnMut() + Send + 'static) -> Self {
        Self(tokio::spawn(async move {
            loop {
                step();
                tokio::time::sleep(INTERVAL).await;
            }
        }))
    }

    /// Play a video in `rect`: a photograph that pans on every tick.
    fn animate(server: &TestServer, rect: Rectangle) -> Self {
        let platform = server.platform.clone();
        Self::start(move || platform.animate(rect).unwrap())
    }

    /// Move the square on every tick: continuous change that LZ4 sends
    /// well.
    fn advance(server: &TestServer) -> Self {
        let platform = server.platform.clone();
        Self::start(move || platform.advance().unwrap())
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Whether two wire rectangles share a pixel.
fn overlaps(a: &Rect, b: Rectangle) -> bool {
    let b = wire_rect(b);
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// One live stream, as a client holds it.
struct LiveStream {
    dest: Rect,
    decoder: Box<dyn VideoDecoder>,
}

/// What a client sees of the display channel once its start sequence is
/// over: the surface, drawn from draws and from decoded stream frames,
/// and what it was told of streams. Every message is checked as it is
/// applied.
struct Viewer {
    screen: Screen,
    live: HashMap<u32, LiveStream>,
    /// Every `STREAM_CREATE`, in order.
    created: Vec<StreamCreate>,
    /// The `unique_id` of each stream's reports.
    unique_ids: HashMap<u32, u32>,
    /// How many `STREAM_DATA` arrived, and the last.
    frames: usize,
    last_frame: Option<StreamData>,
    /// The ids `STREAM_DESTROY` ended, in order.
    destroyed: Vec<u32>,
    /// How many `STREAM_DESTROY_ALL` arrived.
    destroyed_all: usize,
    /// The box of every draw, and of every draw while a stream lived.
    draws: Vec<Rect>,
    draws_while_streaming: Vec<Rect>,
    /// Messages received since `SET_ACK`, for the `ACK`s.
    received: u32,
}

impl Viewer {
    /// Run the start sequence, and acknowledge messages from then on as
    /// ryll does, a window at a time, so that the ack window never holds
    /// frames back.
    async fn start(server: &TestServer, client: &mut TestClient) -> Self {
        let screen = start(server, client, image_compression::AUTO_GLZ).await;
        client
            .send(display_client::ACK_SYNC, &body(&AckSync { generation: 1 }))
            .await;
        // SET_ACK and the start sequence's four.
        Self::resume(screen, 5)
    }

    /// Carry on from `screen`, `received` messages after `SET_ACK`.
    fn resume(screen: Screen, received: u32) -> Self {
        Self {
            screen,
            live: HashMap::new(),
            created: Vec::new(),
            unique_ids: HashMap::new(),
            frames: 0,
            last_frame: None,
            destroyed: Vec::new(),
            destroyed_all: 0,
            draws: Vec::new(),
            draws_while_streaming: Vec::new(),
            received,
        }
    }

    /// Acknowledge `message` if it completes a window, and apply it.
    async fn take(&mut self, client: &mut TestClient, message: &ReceivedMessage) {
        self.received += 1;
        if self.received.is_multiple_of(WINDOW) {
            client.send(display_client::ACK, &[]).await;
        }
        self.apply(message);
    }

    /// Apply one message, checking it.
    fn apply(&mut self, message: &ReceivedMessage) {
        match message.header.message_type {
            display_server::DRAW_COPY => {
                let draw = decode_draw(message);
                self.screen.apply(&draw);
                if !self.live.is_empty() {
                    self.draws_while_streaming.push(draw.bbox);
                }
                self.draws.push(draw.bbox);
            }
            display_server::MARK => assert!(message.payload().is_empty()),
            display_server::STREAM_CREATE => {
                let create = StreamCreate::decode(message.payload()).unwrap();
                let (width, height) = (
                    (create.dest.right - create.dest.left) as u32,
                    (create.dest.bottom - create.dest.top) as u32,
                );
                assert_eq!(
                    create,
                    StreamCreate {
                        surface_id: 0,
                        id: create.id,
                        flags: stream_flags::TOP_DOWN,
                        codec_type: 3,
                        stamp: 0,
                        stream_width: width,
                        stream_height: height,
                        src_width: width,
                        src_height: height,
                        dest: create.dest,
                        clip: Clip::none(),
                    }
                );
                let decoder = for_stream(3, Arc::new(JpegDecoderRsDecoder::new())).unwrap();
                let stream = LiveStream {
                    dest: create.dest,
                    decoder,
                };
                assert!(
                    self.live.insert(create.id, stream).is_none(),
                    "stream {} was created twice",
                    create.id
                );
                self.created.push(create);
            }
            display_server::STREAM_ACTIVATE_REPORT => {
                let activate = StreamActivateReport::decode(message.payload()).unwrap();
                assert!(self.live.contains_key(&activate.stream_id));
                assert_eq!((activate.max_window_size, activate.timeout_ms), (5, 1000));
                self.unique_ids
                    .insert(activate.stream_id, activate.unique_id);
            }
            display_server::STREAM_DATA => {
                let data = StreamData::decode(message.payload()).unwrap();
                let stream = self
                    .live
                    .get_mut(&data.base.id)
                    .expect("a frame for a stream that is not live");
                if let Some(frame) = stream.decoder.decode(&data.data).unwrap() {
                    let dest = stream.dest;
                    let width = (dest.right - dest.left) as usize;
                    assert_eq!(
                        (frame.width as usize, frame.height as i32),
                        (width, dest.bottom - dest.top)
                    );
                    for (row, y) in (dest.top..dest.bottom).enumerate() {
                        let at = (y as usize * self.screen.width as usize + dest.left as usize) * 4;
                        let from = &frame.rgba[row * width * 4..(row + 1) * width * 4];
                        for (to, pixel) in self.screen.rgba[at..at + width * 4]
                            .as_chunks_mut::<4>()
                            .0
                            .iter_mut()
                            .zip(from.as_chunks::<4>().0)
                        {
                            *to = [pixel[0], pixel[1], pixel[2], 255];
                        }
                    }
                }
                self.frames += 1;
                self.last_frame = Some(data);
            }
            display_server::STREAM_DESTROY => {
                let destroy = StreamDestroy::decode(message.payload()).unwrap();
                assert!(
                    self.live.remove(&destroy.id).is_some(),
                    "stream {} ended but was not live",
                    destroy.id
                );
                self.destroyed.push(destroy.id);
            }
            display_server::STREAM_DESTROY_ALL => {
                assert!(message.payload().is_empty());
                self.live.clear();
                self.destroyed_all += 1;
            }
            other => panic!("unexpected opcode {other}"),
        }
    }

    /// Apply whatever arrives within `wait`.
    async fn read_for(&mut self, client: &mut TestClient, wait: Duration) {
        let deadline = Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match client.recv_within(left).await {
                Some(message) => self.take(client, &message).await,
                None => return,
            }
        }
    }

    /// Apply messages until `done` holds.
    async fn read_until(
        &mut self,
        client: &mut TestClient,
        what: &str,
        done: impl Fn(&Self) -> bool,
    ) {
        tokio::time::timeout(support::WAIT, async {
            while !done(self) {
                let message = client.recv().await;
                self.take(client, &message).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
    }

    /// The mean difference, over the red, green and blue of every pixel of
    /// `rect`, between the screen and `desktop`.
    fn error(&self, desktop: &Frame, rect: Rectangle) -> f64 {
        let mut total = 0u64;
        for y in rect.y..rect.y + rect.height {
            let want = rgba(desktop.row(y).unwrap());
            let at = y as usize * self.screen.width as usize * 4;
            let got = &self.screen.rgba[at..at + want.len()];
            let (left, right) = (rect.x as usize * 4, (rect.x + rect.width) as usize * 4);
            for (w, g) in want[left..right]
                .as_chunks::<4>()
                .0
                .iter()
                .zip(got[left..right].as_chunks::<4>().0)
            {
                total += (0..3).map(|c| u64::from(w[c].abs_diff(g[c]))).sum::<u64>();
            }
        }
        total as f64 / (rect.area() * 3) as f64
    }
}

/// Play a video in `rect` until a stream starts and sends a frame.
/// Returns the video, still playing, and the stream's `STREAM_CREATE`.
async fn stream_video(
    server: &TestServer,
    client: &mut TestClient,
    viewer: &mut Viewer,
    rect: Rectangle,
) -> (Ticker, StreamCreate) {
    let video = Ticker::animate(server, rect);
    let streams = viewer.created.len();
    viewer
        .read_until(client, "a stream starts", |viewer| {
            viewer.created.len() > streams
        })
        .await;
    let create = viewer.created[streams].clone();
    let frames = viewer.frames;
    viewer
        .read_until(client, "a stream frame arrives", |viewer| {
            viewer.frames > frames
        })
        .await;
    (video, create)
}

/// A `STREAM_REPORT` for `stream_id` with `unique_id`, of five frames, one
/// dropped.
fn stream_report(stream_id: u32, unique_id: u32) -> StreamReport {
    StreamReport {
        stream_id,
        unique_id,
        start_frame_mm_time: 1000,
        end_frame_mm_time: 1100,
        num_frames: 5,
        num_drops: 1,
        last_frame_delay: 30,
        audio_delay: u32::MAX,
    }
}

/// The report spice-gtk sends for a stream it cannot decode: no frames,
/// `u32::MAX` dropped, and the rest zero.
fn cannot_decode(stream_id: u32) -> StreamReport {
    StreamReport {
        stream_id,
        unique_id: 0,
        start_frame_mm_time: 0,
        end_frame_mm_time: 0,
        num_frames: 0,
        num_drops: u32::MAX,
        last_frame_delay: 0,
        audio_delay: 0,
    }
}

/// What the server logs, for tests that read it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// Capture what this thread logs at `level` and above, until the
    /// guard is dropped. The server's tasks run on the thread of a
    /// `current_thread` test, so this sees what they log.
    fn start(level: tracing::Level) -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(level)
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (logs, guard)
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// What andris warns once when streams are turned off.
const STREAMS_OFF: &str = "sending no more video streams on this connection";

#[tokio::test]
async fn a_playing_video_streams_and_its_frames_decode() {
    let (server, mut main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let message = main.recv().await;
    assert_eq!(message.header.message_type, main_server::INIT);
    let init = MainInit::decode(message.payload()).unwrap();
    let mut viewer = Viewer::start(&server, &mut client).await;

    let started = Instant::now();
    let (video, create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    // Twenty frames at least, a frame interval apart.
    assert!(started.elapsed() >= INTERVAL * 19);
    assert_eq!(create.dest, wire_rect(VIDEO));
    viewer
        .read_until(&mut client, "frames flow", |viewer| viewer.frames >= 3)
        .await;
    drop(video);
    viewer.read_for(&mut client, QUIET).await;

    // A frame at a time, so that each frame sent shows the desktop as it
    // is: decoded, it is close to the desktop, and it is stamped ahead of
    // the session's clock, the one MAIN_INIT gave, by the budget.
    let session = server.handle.session().unwrap();
    let budget = STREAM_LATENCY_BUDGET.as_millis() as u32;
    let mut checked = 0;
    for _ in 0..40 {
        let asked = session.elapsed();
        server.platform.animate(VIDEO).unwrap();
        let frames = viewer.frames;
        viewer
            .read_for(&mut client, Duration::from_millis(150))
            .await;
        let arrived = session.elapsed();
        if viewer.frames == frames {
            // The encoder skipped it to hold its bitrate.
            continue;
        }
        assert_eq!(viewer.frames, frames + 1);
        let time = viewer.last_frame.as_ref().unwrap().base.multi_media_time;
        assert!(init.multi_media_time + budget <= time);
        let earliest = asked.as_millis() as u32 + budget;
        let latest = arrived.as_millis() as u32 + budget;
        assert!(
            (earliest..=latest).contains(&time),
            "a frame stamped {time} ms, asked for at {} ms and read by {} ms",
            asked.as_millis(),
            arrived.as_millis()
        );
        let error = viewer.error(&server.platform.desktop().unwrap(), VIDEO);
        assert!(
            error < STREAM_TOLERANCE,
            "the frame differs from the desktop by {error:.2} a channel on average"
        );
        checked += 1;
        if checked == 3 {
            break;
        }
    }
    assert_eq!(checked, 3, "too few frames were sent");
    assert_eq!(viewer.created.len(), 1);
    assert!(viewer.destroyed.is_empty());
}

#[tokio::test]
async fn draws_flow_beside_a_stream_and_never_inside_it() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let _square = Ticker::advance(&server);
    let (_video, create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    assert_eq!(create.dest, wire_rect(VIDEO));
    viewer.read_for(&mut client, Duration::from_secs(1)).await;

    let inside: Vec<&Rect> = viewer
        .draws_while_streaming
        .iter()
        .filter(|bbox| overlaps(bbox, VIDEO))
        .collect();
    assert!(inside.is_empty(), "draws inside the stream: {inside:?}");
    let beside = viewer
        .draws_while_streaming
        .iter()
        .filter(|bbox| overlaps(bbox, SQUARE_PATH))
        .count();
    assert!(beside >= 10, "only {beside} draws of the square");
    // One stream, of the video, and it lives on.
    assert_eq!(viewer.created.len(), 1);
    assert!(viewer.destroyed.is_empty());
}

#[tokio::test]
async fn continuous_lossless_change_never_streams() {
    // Two tiles wide, so that the square bounces within the same 2 x 2
    // tiles, changing every one of them on every tick: as hot as a video,
    // but not a picture.
    let narrow = Size {
        width: 128,
        height: 192,
    };
    let display = DisplayOptions {
        frame_interval: INTERVAL,
        ..display_options()
    };
    let server = TestServer::start_sized(narrow, display).await;
    let (server, _main, mut client) = open_display_on(server, capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let square = Ticker::advance(&server);
    viewer
        .read_for(&mut client, Duration::from_millis(1500))
        .await;
    drop(square);
    assert!(viewer.created.is_empty());
    assert!(
        viewer.draws.len() >= 30,
        "only {} draws",
        viewer.draws.len()
    );
}

#[tokio::test]
async fn a_still_stream_ends_and_its_area_is_repaired_at_once() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (video, create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    drop(video);
    let stopped = Instant::now();
    viewer
        .read_until(&mut client, "the stream ends", |viewer| {
            !viewer.destroyed.is_empty()
        })
        .await;
    let took = stopped.elapsed();
    assert!(
        took >= STREAM_QUIET - INTERVAL * 2 && took < Duration::from_millis(1500),
        "the stream ended {took:?} after the video stopped"
    );
    assert_eq!(viewer.destroyed, vec![create.id]);

    // The lossless refresh follows at once, and the client then shows
    // the desktop exactly.
    viewer
        .read_for(&mut client, Duration::from_millis(250))
        .await;
    viewer
        .screen
        .assert_shows(&server.platform.desktop().unwrap());
    viewer.read_for(&mut client, QUIET).await;
    assert_eq!(viewer.created.len(), 1);
}

#[tokio::test]
async fn a_video_that_moves_away_ends_its_stream_and_another_starts() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (video, first) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    drop(video);
    let (_video, second) = stream_video(&server, &mut client, &mut viewer, MOVED).await;
    assert_eq!(viewer.destroyed, vec![first.id]);
    assert_eq!(second.dest, wire_rect(MOVED));
    assert_eq!(second.id, first.id + 1);
}

#[tokio::test]
async fn a_video_that_grows_past_its_stream_restarts_it() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let small = Rectangle::new(320, 192, 128, 128);
    let (video, first) = stream_video(&server, &mut client, &mut viewer, small).await;
    assert_eq!(first.dest, wire_rect(small));
    drop(video);
    // Three times the area, around it: it never goes still.
    let large = Rectangle::new(256, 192, 384, 192);
    let grown = Instant::now();
    let (_video, second) = stream_video(&server, &mut client, &mut viewer, large).await;
    assert!(grown.elapsed() < STREAM_QUIET + Duration::from_secs(1));
    assert_eq!(viewer.destroyed, vec![first.id]);
    assert_eq!(second.dest, wire_rect(large));
}

#[tokio::test]
async fn no_stream_starts_without_both_h264_caps() {
    for caps in [
        capabilities::DEFAULT_DISPLAY & !capabilities::DISPLAY_CODEC_H264,
        capabilities::DEFAULT_DISPLAY & !capabilities::DISPLAY_MULTI_CODEC,
    ] {
        let (server, _main, mut client) = open_wide(caps).await;
        let mut viewer = Viewer::start(&server, &mut client).await;
        let video = Ticker::animate(&server, VIDEO);
        viewer.read_for(&mut client, Duration::from_secs(1)).await;
        drop(video);
        assert!(viewer.created.is_empty());
        // The video goes as draws instead.
        let drawn = viewer
            .draws
            .iter()
            .filter(|bbox| overlaps(bbox, VIDEO))
            .count();
        assert!(drawn >= 20, "only {drawn} draws of the video");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reports_for_another_stream_or_unique_id_are_ignored() {
    let (logs, _guard) = Captured::start(tracing::Level::DEBUG);
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (_video, create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    let (id, unique_id) = (create.id, viewer.unique_ids[&create.id]);

    // Another stream's report, even one saying it cannot be decoded, and
    // a report from an earlier stream with this id.
    for report in [
        cannot_decode(id + 1),
        stream_report(id + 1, unique_id),
        stream_report(id, unique_id.wrapping_add(1)),
        stream_report(id, unique_id),
    ] {
        client
            .send(display_client::STREAM_REPORT, &body(&report))
            .await;
    }
    let reports = |logs: &Captured| logs.text().matches("a stream report").count();
    viewer
        .read_until(&mut client, "the reports are read", |_| reports(&logs) == 4)
        .await;
    let text = logs.text();
    assert_eq!(
        text.matches("a report for another stream; ignored").count(),
        2,
        "{text}"
    );
    assert_eq!(
        text.matches("a report from an earlier stream with this id; ignored")
            .count(),
        1,
        "{text}"
    );
    assert!(!text.contains(STREAMS_OFF), "{text}");

    // The stream lives on.
    let frames = viewer.frames;
    viewer
        .read_until(&mut client, "more frames arrive", |viewer| {
            viewer.frames > frames
        })
        .await;
    assert!(viewer.destroyed.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_that_cannot_decode_ends_streams_with_one_warning() {
    let (logs, _guard) = Captured::start(tracing::Level::WARN);
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (_video, create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;

    // As spice-gtk sends it, with no unique_id.
    client
        .send(
            display_client::STREAM_REPORT,
            &body(&cannot_decode(create.id)),
        )
        .await;
    viewer
        .read_until(&mut client, "the stream ends", |viewer| {
            !viewer.destroyed.is_empty()
        })
        .await;
    assert_eq!(viewer.destroyed, vec![create.id]);

    // Saying it again does nothing, and the video, still playing, goes
    // as draws from now on.
    client
        .send(
            display_client::STREAM_REPORT,
            &body(&cannot_decode(create.id)),
        )
        .await;
    let draws = viewer.draws.len();
    viewer.read_for(&mut client, Duration::from_secs(1)).await;
    assert_eq!(viewer.created.len(), 1);
    assert!(viewer.draws.len() >= draws + 20);
    let text = logs.text();
    assert_eq!(text.matches(STREAMS_OFF).count(), 1, "{text}");
}

#[tokio::test]
async fn a_resize_ends_every_stream_before_the_surface() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (video, first) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    drop(video);

    let size = Size {
        width: 640,
        height: 448,
    };
    server.platform.set_size(size).unwrap();
    viewer
        .read_until(&mut client, "every stream ends", |viewer| {
            viewer.destroyed_all == 1
        })
        .await;
    assert!(viewer.destroyed.is_empty());
    // Next comes the surface, in five messages.
    let screen = expect_replaced_surface(&server, &mut client, size).await;

    // Another stream may start on the new surface, with the next id.
    let mut viewer = Viewer::resume(screen, viewer.received + 5);
    let (_video, second) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    assert_eq!(second.id, first.id + 1);
}
