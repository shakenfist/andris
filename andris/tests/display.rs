//! The display channel: its start sequence, the bands it sends and how
//! they decode, compression choice, the ack window and resizing, against
//! an in-process andris on the synthetic platform.
//!
//! Every band is decoded, with ryll's `DrawCopy` reader and LZ4 decoder,
//! onto a client-side copy of the surface, which is compared with what
//! the synthetic desktop shows.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use andris::platform::{Frame, FrameSource, Size};
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

/// The frame interval of every server here: short, so tests are quick.
const INTERVAL: Duration = Duration::from_millis(20);

/// How long to wait before concluding the server sends nothing: five
/// frame intervals.
const QUIET: Duration = Duration::from_millis(100);

/// The ack window andris asks for.
const WINDOW: u32 = 20;

fn body<T: WireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write(&mut out);
    out
}

/// What the client knows of the primary surface: its size and RGBA
/// pixels, drawn from the bands it was sent.
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
    assert_eq!(bbox.left, 0, "bands span the full width");
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
        "the image descriptor is the band's size"
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
    let server = TestServer::start_with_frame_interval(INTERVAL).await;
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

/// Read a complete frame of `size` onto `screen`: one DRAW_COPY per
/// 64-row band, top first. Returns each band's image type and id.
async fn expect_complete_frame(client: &mut TestClient, screen: &mut Screen) -> Vec<(u8, u64)> {
    let mut images = Vec::new();
    for top in (0..screen.height).step_by(64) {
        let draw = decode_draw(&client.recv().await);
        let rows = 64.min(screen.height - top);
        assert_eq!(
            draw.bbox,
            Rect {
                top: top as i32,
                left: 0,
                bottom: (top + rows) as i32,
                right: screen.width as i32,
            }
        );
        screen.apply(&draw);
        images.push((draw.image_type, draw.image_id));
    }
    images
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
    if let Some(message) = client.recv_within(QUIET).await {
        panic!(
            "expected nothing, but got opcode {}",
            message.header.message_type
        );
    }
}

/// Run the whole start sequence with ryll's default capabilities, and
/// return the screen it drew.
async fn start(server: &TestServer, client: &mut TestClient, compression: u8) -> Screen {
    expect_set_ack(client).await;
    send_init(client, compression).await;
    let size = server.platform.capture().unwrap().size();
    expect_surface_create(client, size).await;
    let mut screen = Screen::new(size);
    expect_complete_frame(client, &mut screen).await;
    expect_monitors_config(client, size).await;
    expect_mark(client).await;
    screen
}

/// Move the square, and read the two bands it crosses onto `screen`.
/// Returns each band's image type and id.
async fn advance_and_expect_bands(
    server: &TestServer,
    client: &mut TestClient,
    screen: &mut Screen,
) -> Vec<(u8, u64)> {
    server.platform.advance().unwrap();
    let mut images = Vec::new();
    for top in [64, 128] {
        let draw = decode_draw(&client.recv().await);
        assert_eq!(draw.bbox.top, top, "only the square's bands are sent");
        screen.apply(&draw);
        images.push((draw.image_type, draw.image_id));
    }
    screen.assert_shows(&server.platform.capture().unwrap());
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
    assert_eq!(images, vec![(lz4, 1), (lz4, 2), (lz4, 3)]);
    screen.assert_shows(&server.platform.capture().unwrap());
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
    let size = server.platform.capture().unwrap().size();
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    expect_complete_frame(&mut client, &mut screen).await;
    expect_mark(&mut client).await;
}

#[tokio::test]
async fn advance_sends_only_the_bands_the_square_crossed() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let lz4 = ImageType::Lz4 as u8;
    for (first, second) in [(4, 5), (6, 7), (8, 9)] {
        let images = advance_and_expect_bands(&server, &mut client, &mut screen).await;
        assert_eq!(images, vec![(lz4, first), (lz4, second)]);
    }
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn bitmaps_for_a_client_without_lz4() {
    let caps = capabilities::DEFAULT_DISPLAY & !capabilities::DISPLAY_LZ4_COMPRESSION;
    let (server, _main, mut client) = open_display(caps).await;
    expect_set_ack(&mut client).await;
    send_init(&mut client, image_compression::LZ4).await;
    let size = server.platform.capture().unwrap().size();
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    let pixmap = ImageType::Pixmap as u8;
    let images = expect_complete_frame(&mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == pixmap));
    screen.assert_shows(&server.platform.capture().unwrap());
    expect_monitors_config(&mut client, size).await;
    expect_mark(&mut client).await;

    let images = advance_and_expect_bands(&server, &mut client, &mut screen).await;
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
    let size = server.platform.capture().unwrap().size();
    expect_surface_create(&mut client, size).await;
    let mut screen = Screen::new(size);
    let images = expect_complete_frame(&mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == pixmap));
    screen.assert_shows(&server.platform.capture().unwrap());
    expect_monitors_config(&mut client, size).await;
    expect_mark(&mut client).await;

    // The server reads a message within a tick; give it one before the
    // desktop changes.
    send_compression(&mut client, image_compression::AUTO_GLZ).await;
    tokio::time::sleep(INTERVAL).await;
    let images = advance_and_expect_bands(&server, &mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == lz4));

    send_compression(&mut client, image_compression::OFF).await;
    tokio::time::sleep(INTERVAL).await;
    let images = advance_and_expect_bands(&server, &mut client, &mut screen).await;
    assert!(images.iter().all(|&(image_type, _)| image_type == pixmap));
}

#[tokio::test]
async fn nothing_is_sent_beyond_twice_the_ack_window_until_acks_arrive() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    // SET_ACK, SURFACE_CREATE, three bands, MONITORS_CONFIG and MARK.
    let mut screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    let mut unacked = 7;

    // Two bands a step, until the server has sent one more than twice
    // the window, as many as it may.
    let limit = 2 * WINDOW + 1;
    while unacked < limit {
        advance_and_expect_bands(&server, &mut client, &mut screen).await;
        unacked += 2;
    }
    assert_eq!(unacked, limit);

    // The desktop changes, but the server holds back.
    server.platform.advance().unwrap();
    expect_quiet(&mut client).await;

    // One window acknowledged: the latest frame's bands follow, and no
    // ticks were queued meanwhile.
    client
        .send(display_client::ACK_SYNC, &body(&AckSync { generation: 1 }))
        .await;
    client.send(display_client::ACK, &[]).await;
    for top in [64, 128] {
        let draw = decode_draw(&client.recv().await);
        assert_eq!(draw.bbox.top, top);
        screen.apply(&draw);
    }
    screen.assert_shows(&server.platform.capture().unwrap());
    expect_quiet(&mut client).await;
}

#[tokio::test]
async fn a_size_change_replaces_the_surface() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    let screen = start(&server, &mut client, image_compression::AUTO_GLZ).await;
    assert_eq!(screen.height, 192);

    // 200 rows: the last band is 8 rows.
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
    let images = expect_complete_frame(&mut client, &mut screen).await;
    assert_eq!(images.len(), 4);
    screen.assert_shows(&server.platform.capture().unwrap());
    expect_monitors_config(&mut client, size).await;
    expect_mark(&mut client).await;
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
    advance_and_expect_bands(&server, &mut client, &mut screen).await;
}

#[tokio::test]
async fn a_malformed_stream_report_closes_the_channel() {
    let (server, _main, mut client) = open_display(capabilities::DEFAULT_DISPLAY).await;
    start(&server, &mut client, image_compression::AUTO_GLZ).await;
    client.send(display_client::STREAM_REPORT, &[0; 4]).await;
    client.expect_closed().await;
}
