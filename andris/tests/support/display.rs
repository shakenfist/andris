//! A display channel's client side, shared by the display and stream
//! tests: its start sequence, and draws decoded with ryll's `DrawCopy`
//! reader and its LZ4 and JPEG decoders onto a client-side copy of the
//! surface, which is compared with what the synthetic desktop shows.

use std::time::Duration;

use andris::platform::{DisplayGeometry, Frame, Rectangle, Size};
use shakenfist_spice_compression::{JpegDecoder, JpegDecoderRsDecoder, decompress_spice_lz4};
use shakenfist_spice_protocol::constants::{
    bitmap_flags, bitmap_fmt, clip_type, ropd, surface_fmt,
};
use shakenfist_spice_protocol::messages::{
    DisplayHead, DisplayInit, DisplayMonitorsConfig, DrawCopy, ImagePayload, PreferredCompression,
    PreferredVideoCodecType, ReceivedMessage, Rect, SetAck, SurfaceCreate, SurfaceDestroy,
    WireType,
};
use shakenfist_spice_protocol::{ChannelType, ImageType, display_client, display_server};

use super::{TestClient, TestServer};

/// The frame interval of the servers here, unless a test says otherwise:
/// short, so tests are quick.
pub const INTERVAL: Duration = Duration::from_millis(20);

/// How long to wait before concluding the server sends nothing: five
/// frame intervals.
pub const QUIET: Duration = Duration::from_millis(100);

/// The ack window andris asks for.
pub const WINDOW: u32 = 20;

pub fn body<T: WireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write(&mut out);
    out
}

/// What the client knows of the primary surface: its size and RGBA
/// pixels, drawn from the draws it was sent.
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// One decoded DRAW_COPY.
pub struct Draw {
    pub bbox: Rect,
    pub image_type: u8,
    pub image_id: u64,
    /// The image as RGBA, `width * height` pixels.
    pub rgba: Vec<u8>,
}

impl Screen {
    pub fn new(size: Size) -> Self {
        Self {
            width: size.width,
            height: size.height,
            rgba: vec![0; size.width as usize * size.height as usize * 4],
        }
    }

    /// Put `draw`'s image at its box.
    pub fn apply(&mut self, draw: &Draw) {
        let left = draw.bbox.left as usize;
        let width = (draw.bbox.right - draw.bbox.left) as usize;
        for (row, y) in (draw.bbox.top..draw.bbox.bottom).enumerate() {
            let at = (y as usize * self.width as usize + left) * 4;
            let src = &draw.rgba[row * width * 4..(row + 1) * width * 4];
            self.rgba[at..at + width * 4].copy_from_slice(src);
        }
    }

    /// Assert the screen shows exactly `frame`.
    pub fn assert_shows(&self, frame: &Frame) {
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
pub fn rgba(bgrx: &[u8]) -> Vec<u8> {
    bgrx.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect()
}

/// Decode a DRAW_COPY, checking every field andris fixes: surface 0, no
/// clip, `OP_PUT`, scale mode 0, no mask, a source area of the whole
/// image, and an image exactly the box's size with no flags.
pub fn decode_draw(message: &ReceivedMessage) -> Draw {
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
pub async fn open_display(caps: u32) -> (TestServer, TestClient, TestClient) {
    open_display_on(TestServer::start_with_frame_interval(INTERVAL).await, caps).await
}

/// [`open_display`] on `server`.
pub async fn open_display_on(
    server: TestServer,
    caps: u32,
) -> (TestServer, TestClient, TestClient) {
    let (main, session) = server.open_main().await;
    let display = server
        .open_with_caps(session, ChannelType::Display, caps)
        .await;
    (server, main, display)
}

/// Read `SET_ACK` and check it.
pub async fn expect_set_ack(client: &mut TestClient) {
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
pub async fn send_init(client: &mut TestClient, compression: u8) {
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

pub async fn send_compression(client: &mut TestClient, compression: u8) {
    let preferred = PreferredCompression {
        image_compression: compression,
    };
    client
        .send(display_client::PREFERRED_COMPRESSION, &body(&preferred))
        .await;
}

/// Read `SURFACE_CREATE` and check it creates the primary surface at
/// `size`.
pub async fn expect_surface_create(client: &mut TestClient, size: Size) {
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
pub fn wire_rect(rect: Rectangle) -> Rect {
    Rect {
        top: rect.y as i32,
        left: rect.x as i32,
        bottom: (rect.y + rect.height) as i32,
        right: (rect.x + rect.width) as i32,
    }
}

/// Read one DRAW_COPY for each of `expected`, in order, onto `screen`.
/// Returns each draw's image type and id.
pub async fn expect_draws(
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
pub async fn expect_complete_frame(client: &mut TestClient, screen: &mut Screen) -> Vec<(u8, u64)> {
    let size = Size {
        width: screen.width,
        height: screen.height,
    };
    expect_draws(client, screen, &[Rectangle::full(size)]).await
}

/// Read `MONITORS_CONFIG` and check it has one head of `size`.
pub async fn expect_monitors_config(client: &mut TestClient, size: Size) {
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

pub async fn expect_mark(client: &mut TestClient) {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, display_server::MARK);
    assert!(message.payload().is_empty());
}

pub async fn expect_quiet(client: &mut TestClient) {
    expect_quiet_for(client, QUIET).await;
}

pub async fn expect_quiet_for(client: &mut TestClient, wait: Duration) {
    if let Some(message) = client.recv_within(wait).await {
        panic!(
            "expected nothing, but got opcode {}",
            message.header.message_type
        );
    }
}

/// Read one frame onto `screen`: DRAW_COPYs until a `MARK`. Returns the
/// draws' boxes.
pub async fn expect_frame(client: &mut TestClient, screen: &mut Screen) -> Vec<Rect> {
    expect_frame_images(client, screen)
        .await
        .into_iter()
        .map(|(bbox, _)| bbox)
        .collect()
}

/// [`expect_frame`], returning each draw's box and image type.
pub async fn expect_frame_images(client: &mut TestClient, screen: &mut Screen) -> Vec<(Rect, u8)> {
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
pub async fn start(server: &TestServer, client: &mut TestClient, compression: u8) -> Screen {
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

/// Expect the surface to be replaced by one of `size`, and the new
/// surface's first frame to show the desktop exactly.
pub async fn expect_replaced_surface(
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
