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

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use andris::platform::synthetic::{SQUARE_SIZE, SQUARE_TOP};
use andris::platform::{DisplayGeometry, Frame, Rectangle, Region, Size};
use andris::server::display::{CAPTURE_RETRY, DisplayOptions, MAX_CAPTURE_FAILURES};
use andris::server::updates::{
    JpegQuality, MAX_RECTS_PER_FRAME, STREAM_LATENCY_BUDGET, StreamRequest,
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

/// The area the stream tests stream: two tiles by one and a half, across
/// three colour bars and the square's path.
const STREAM_RECT: Rectangle = Rectangle::new(64, 64, 128, 96);

/// How long to wait for a stream frame before asking again: the encoder
/// may skip a frame to hold its bitrate, and then sends nothing.
const SKIPPED: Duration = Duration::from_secs(1);

/// The most a decoded stream frame may differ from the desktop, as a mean
/// over its pixels' red, green and blue.
const STREAM_TOLERANCE: f64 = 3.0;

/// Ask the display channel for `request`.
fn request_stream(server: &TestServer, request: StreamRequest) {
    let session = server.handle.session().unwrap();
    assert!(
        session.request_stream(request),
        "no display channel took {request:?}"
    );
}

/// Ask for a stream over `rect`, and read its `STREAM_CREATE` and
/// `STREAM_ACTIVATE_REPORT`, checking both. Returns the stream's id and
/// the `unique_id` its reports must carry.
async fn start_stream(server: &TestServer, client: &mut TestClient, rect: Rectangle) -> (u32, u32) {
    request_stream(server, StreamRequest::Start(rect));
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::STREAM_CREATE);
    let create = StreamCreate::decode(message.payload()).unwrap();
    assert_eq!(
        create,
        StreamCreate {
            surface_id: 0,
            id: create.id,
            flags: stream_flags::TOP_DOWN,
            codec_type: 3,
            stamp: 0,
            stream_width: rect.width,
            stream_height: rect.height,
            src_width: rect.width,
            src_height: rect.height,
            dest: wire_rect(rect),
            clip: Clip::none(),
        }
    );
    let message = client.recv().await;
    assert_eq!(
        message.header.message_type,
        display_server::STREAM_ACTIVATE_REPORT
    );
    let activate = StreamActivateReport::decode(message.payload()).unwrap();
    assert_eq!(
        activate,
        StreamActivateReport {
            stream_id: create.id,
            unique_id: activate.unique_id,
            max_window_size: 5,
            timeout_ms: 1000,
        }
    );
    (create.id, activate.unique_id)
}

/// Ask for stream frames until one arrives, since the encoder may skip
/// some, and check it is for stream `id`. Returns it, with the session's
/// time just before it was asked for and just after it arrived.
async fn next_stream_frame(
    server: &TestServer,
    client: &mut TestClient,
    id: u32,
) -> (StreamData, Duration, Duration) {
    let session = server.handle.session().unwrap();
    for _ in 0..10 {
        let asked = session.elapsed();
        request_stream(server, StreamRequest::Frame);
        if let Some(message) = client.recv_within(SKIPPED).await {
            let arrived = session.elapsed();
            assert_eq!(message.header.message_type, display_server::STREAM_DATA);
            let data = StreamData::decode(message.payload()).unwrap();
            assert_eq!(data.base.id, id);
            return (data, asked, arrived);
        }
    }
    panic!("ten frames asked for, and none sent");
}

/// Read `STREAM_DESTROY`, and check it ends stream `id`.
async fn expect_stream_destroy(client: &mut TestClient, id: u32) {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::STREAM_DESTROY);
    assert_eq!(
        StreamDestroy::decode(message.payload()).unwrap(),
        StreamDestroy { id }
    );
}

/// The decoder ryll makes for an H.264 stream.
fn stream_decoder() -> Box<dyn VideoDecoder> {
    for_stream(3, Arc::new(JpegDecoderRsDecoder::new())).unwrap()
}

/// Decode `data` and check it shows what the desktop shows in `rect`, to
/// within [`STREAM_TOLERANCE`].
fn assert_frame_shows(
    decoder: &mut Box<dyn VideoDecoder>,
    data: &StreamData,
    server: &TestServer,
    rect: Rectangle,
) {
    let decoded = decoder
        .decode(&data.data)
        .unwrap()
        .expect("a whole frame decodes");
    assert_eq!((decoded.width, decoded.height), (rect.width, rect.height));
    let expected = server.platform.desktop().unwrap().crop(rect).unwrap();
    let mut total = 0u64;
    for y in 0..rect.height {
        let want = rgba(expected.row(y).unwrap());
        let at = y as usize * rect.width as usize * 4;
        let got = &decoded.rgba[at..at + want.len()];
        for (w, g) in want.as_chunks::<4>().0.iter().zip(got.as_chunks::<4>().0) {
            total += (0..3).map(|c| u64::from(w[c].abs_diff(g[c]))).sum::<u64>();
        }
    }
    let error = total as f64 / (rect.area() * 3) as f64;
    assert!(
        error < STREAM_TOLERANCE,
        "the frame differs from the desktop by {error:.2} a channel on average"
    );
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
async fn a_stream_is_created_sends_frames_that_decode_and_is_destroyed() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let (id, _) = start_stream(&server, &mut client, STREAM_RECT).await;
    // Nothing is sent until a frame is asked for.
    expect_quiet(&mut client).await;

    let mut decoder = stream_decoder();
    let (data, _, _) = next_stream_frame(&server, &mut client, id).await;
    assert_frame_shows(&mut decoder, &data, &server, STREAM_RECT);

    // Draws carry on as usual, and the next frame shows the change too.
    advance_and_expect_frame(&server, &mut client, &mut screen).await;
    let (data, _, _) = next_stream_frame(&server, &mut client, id).await;
    assert_frame_shows(&mut decoder, &data, &server, STREAM_RECT);

    request_stream(&server, StreamRequest::Stop);
    expect_stream_destroy(&mut client, id).await;
    // With no stream, a frame asked for sends nothing.
    request_stream(&server, StreamRequest::Frame);
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn stream_frames_are_stamped_ahead_of_the_session_clock_by_the_budget() {
    let (server, mut main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let message = main.recv().await;
    assert_eq!(message.header.message_type, main_server::INIT);
    let init = MainInit::decode(message.payload()).unwrap();
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let (id, _) = start_stream(&server, &mut client, STREAM_RECT).await;

    let budget = STREAM_LATENCY_BUDGET.as_millis() as u32;
    for _ in 0..3 {
        let (data, asked, arrived) = next_stream_frame(&server, &mut client, id).await;
        let time = data.base.multi_media_time;
        // On the clock MAIN_INIT gave the client, between the moment it
        // was asked for and the moment it arrived, plus the budget.
        assert!(init.multi_media_time + budget <= time);
        let earliest = asked.as_millis() as u32 + budget;
        let latest = arrived.as_millis() as u32 + budget;
        assert!(
            (earliest..=latest).contains(&time),
            "a frame stamped {time} ms, asked for at {} ms and arrived at {} ms",
            asked.as_millis(),
            arrived.as_millis()
        );
    }
}

#[tokio::test]
async fn no_stream_starts_without_both_h264_caps() {
    for caps in [
        capabilities::DEFAULT_DISPLAY & !capabilities::DISPLAY_CODEC_H264,
        capabilities::DEFAULT_DISPLAY & !capabilities::DISPLAY_MULTI_CODEC,
    ] {
        let (server, _main, mut client) = open_display(caps).await;
        let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
        request_stream(&server, StreamRequest::Start(STREAM_RECT));
        request_stream(&server, StreamRequest::Frame);
        request_stream(&server, StreamRequest::Stop);
        expect_quiet(&mut client).await;
        // The channel carries on with draws.
        advance_and_expect_frame(&server, &mut client, &mut screen).await;
        expect_quiet(&mut client).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reports_for_another_stream_or_unique_id_are_ignored() {
    let (logs, _guard) = Captured::start(tracing::Level::DEBUG);
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let (id, unique_id) = start_stream(&server, &mut client, STREAM_RECT).await;

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
    wait_until("the reports are read", || {
        logs.text().matches("a stream report").count() == 4
    })
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
    next_stream_frame(&server, &mut client, id).await;
    expect_quiet(&mut client).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_that_cannot_decode_ends_streams_with_one_warning() {
    let (logs, _guard) = Captured::start(tracing::Level::WARN);
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let (id, _) = start_stream(&server, &mut client, STREAM_RECT).await;
    next_stream_frame(&server, &mut client, id).await;

    // As spice-gtk sends it, with no unique_id.
    client
        .send(display_client::STREAM_REPORT, &body(&cannot_decode(id)))
        .await;
    expect_stream_destroy(&mut client, id).await;

    // Saying it again, or asking for another stream, does nothing.
    client
        .send(display_client::STREAM_REPORT, &body(&cannot_decode(id)))
        .await;
    request_stream(&server, StreamRequest::Start(STREAM_RECT));
    request_stream(&server, StreamRequest::Frame);
    expect_quiet(&mut client).await;

    // Draws carry on.
    advance_and_expect_frame(&server, &mut client, &mut screen).await;
    let text = logs.text();
    assert_eq!(text.matches(STREAMS_OFF).count(), 1, "{text}");
}

#[tokio::test]
async fn a_resize_ends_every_stream_before_the_surface() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let (id, _) = start_stream(&server, &mut client, STREAM_RECT).await;
    next_stream_frame(&server, &mut client, id).await;

    let size = Size {
        width: 320,
        height: 256,
    };
    server.platform.set_size(size).unwrap();
    let message = client.recv().await;
    assert_eq!(
        message.header.message_type,
        display_server::STREAM_DESTROY_ALL
    );
    assert!(message.payload().is_empty());
    expect_replaced_surface(&server, &mut client, size).await;
    expect_quiet(&mut client).await;

    // The stream went with the surface, and another may start on the new
    // one, with the next id.
    request_stream(&server, StreamRequest::Frame);
    expect_quiet(&mut client).await;
    let (next, _) = start_stream(&server, &mut client, STREAM_RECT).await;
    assert_eq!(next, id + 1);
    let mut decoder = stream_decoder();
    let (data, _, _) = next_stream_frame(&server, &mut client, next).await;
    assert_frame_shows(&mut decoder, &data, &server, STREAM_RECT);
}
