//! Video streams on the display channel: when an area becomes a stream,
//! what the stream sends and how it decodes, how it ends and how its area
//! is repaired, the client's reports and rate control, and the metrics,
//! against an in-process andris on the synthetic platform.
//!
//! A `Viewer` plays the client: it applies every draw and every stream
//! frame, decoded with ryll's H.264 stream decoder, to a client-side copy
//! of the surface, and checks each stream message as it goes.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use andris::platform::synthetic::{SQUARE_SIZE, SQUARE_TOP};
use andris::platform::{Frame, Rectangle, Size};
use andris::server::display::DisplayOptions;
use andris::server::updates::{
    CLEAN_PERIOD, HOT_FRAMES, MIN_STREAM_BITRATE_BPS, STREAM_LATENCY_BUDGET, STREAM_QUIET,
};
use shakenfist_spice_compression::{
    JpegDecoderRsDecoder, VideoDecoder, for_stream, target_bitrate_bps,
};
use shakenfist_spice_protocol::constants::{image_compression, stream_flags};
use shakenfist_spice_protocol::messages::{
    AckSync, Clip, MainInit, ReceivedMessage, Rect, StreamActivateReport, StreamCreate, StreamData,
    StreamDestroy, StreamReport, WireType,
};
use shakenfist_spice_protocol::{capabilities, display_client, display_server, main_server};
use tracing_subscriber::fmt::MakeWriter;

use support::display::*;
use support::{TestClient, TestServer, display_options, wait_until};

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
    /// How many `MARK`s arrived: frames of draws.
    marks: usize,
    /// Messages received since `SET_ACK`, for the `ACK`s.
    received: u32,
    /// Whether to send them.
    acking: bool,
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
            marks: 0,
            received,
            acking: true,
        }
    }

    /// Acknowledge `message` if it completes a window, and apply it.
    async fn take(&mut self, client: &mut TestClient, message: &ReceivedMessage) {
        self.received += 1;
        if self.acking && self.received.is_multiple_of(WINDOW) {
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
            display_server::MARK => {
                assert!(message.payload().is_empty());
                self.marks += 1;
            }
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

    /// Apply messages until `done` holds or `wait` passes; return whether
    /// it held. For a condition that may never hold, such as a frame the
    /// encoder may skip.
    async fn read_until_within(
        &mut self,
        client: &mut TestClient,
        wait: Duration,
        done: impl Fn(&Self) -> bool,
    ) -> bool {
        let deadline = Instant::now() + wait;
        while !done(self) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match client.recv_within(left).await {
                Some(message) => self.take(client, &message).await,
                None => return done(self),
            }
        }
        true
    }

    /// Apply messages until `done` holds, waiting up to `WAIT`: a loaded
    /// host makes this slower, never wrong.
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

/// How many frames of draws a test that expects no stream watches for:
/// twice the 20 a stream needs to start, so that a stalled host cannot
/// pass it by sending too little to stream.
const NO_STREAM_FRAMES: usize = 2 * HOT_FRAMES;

/// How long after a stream ends its area must be exact: the lossless
/// refresh is due at once, and the next is due a lossless delay (500 ms)
/// later, so anything under that tells the two apart, with room for a
/// slow host.
const REPAIRED_WITHIN: Duration = Duration::from_millis(400);

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
        // Anything still on its way belongs to the step before.
        viewer.read_for(&mut client, QUIET).await;
        let asked = session.elapsed();
        server.platform.animate(VIDEO).unwrap();
        let frames = viewer.frames;
        let sent = viewer
            .read_until_within(&mut client, Duration::from_millis(800), |viewer| {
                viewer.frames > frames
            })
            .await;
        let arrived = session.elapsed();
        if !sent {
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
    let beside = |viewer: &Viewer| {
        viewer
            .draws_while_streaming
            .iter()
            .filter(|bbox| overlaps(bbox, SQUARE_PATH))
            .count()
    };
    viewer
        .read_until(&mut client, "ten draws of the square", |viewer| {
            beside(viewer) >= 10
        })
        .await;

    let inside: Vec<&Rect> = viewer
        .draws_while_streaming
        .iter()
        .filter(|bbox| overlaps(bbox, VIDEO))
        .collect();
    assert!(inside.is_empty(), "draws inside the stream: {inside:?}");
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
    // Watched for twice the frames a stream needs, however long they take.
    let marks = viewer.marks;
    viewer
        .read_until(
            &mut client,
            "the square has moved enough to stream",
            |viewer| viewer.marks >= marks + NO_STREAM_FRAMES || !viewer.created.is_empty(),
        )
        .await;
    drop(square);
    assert!(viewer.created.is_empty());
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
    viewer.read_for(&mut client, REPAIRED_WITHIN).await;
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

/// The first stream ends because the video outgrew it: andris says so,
/// and it cannot have gone still, since the video plays on over it.
#[tokio::test(flavor = "current_thread")]
async fn a_video_that_grows_past_its_stream_restarts_it() {
    let (logs, _guard) = Captured::start(tracing::Level::INFO);
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let small = Rectangle::new(320, 192, 128, 128);
    let (video, first) = stream_video(&server, &mut client, &mut viewer, small).await;
    assert_eq!(first.dest, wire_rect(small));
    drop(video);
    // Three times the area, around it: it never goes still.
    let large = Rectangle::new(256, 192, 384, 192);
    let (_video, second) = stream_video(&server, &mut client, &mut viewer, large).await;
    assert_eq!(viewer.destroyed, vec![first.id]);
    assert_eq!(second.dest, wire_rect(large));
    let text = logs.text();
    let ends: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("ending a video stream"))
        .collect();
    assert_eq!(ends.len(), 1, "{text}");
    assert!(
        ends[0].contains("a changing area has outgrown it"),
        "{}",
        ends[0]
    );
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
        // The video goes as draws instead: watched for twice the frames a
        // stream needs, however long they take.
        let drawn = |viewer: &Viewer| {
            viewer
                .draws
                .iter()
                .filter(|bbox| overlaps(bbox, VIDEO))
                .count()
        };
        viewer
            .read_until(
                &mut client,
                "the video has played enough to stream",
                |viewer| drawn(viewer) >= NO_STREAM_FRAMES || !viewer.created.is_empty(),
            )
            .await;
        drop(video);
        assert!(viewer.created.is_empty());
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
    viewer
        .read_until(
            &mut client,
            "the video has played enough to stream again",
            |viewer| viewer.draws.len() >= draws + NO_STREAM_FRAMES || viewer.created.len() > 1,
        )
        .await;
    assert_eq!(viewer.created.len(), 1);
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

/// The live stream's target bitrate, as the metrics hold it; 0 for none.
fn stream_target(server: &TestServer) -> u64 {
    server.handle.session().unwrap().stats().stream_target_bps
}

/// The bitrate a stream over [`VIDEO`] starts at, and never exceeds:
/// ryll's formula at the 50 frames a second of [`INTERVAL`], which the
/// 1 Mbit/s floor holds up.
fn video_cap() -> u64 {
    let cap = target_bitrate_bps(VIDEO.width, VIDEO.height, 50);
    assert_eq!(cap, 1_000_000);
    u64::from(cap)
}

/// A report on stream `id` of five frames with `drops` late, the last
/// with `last_frame_delay` ms to spare.
fn report_on(id: u32, unique_id: u32, drops: u32, last_frame_delay: i32) -> StreamReport {
    StreamReport {
        stream_id: id,
        unique_id,
        start_frame_mm_time: 0,
        end_frame_mm_time: 100,
        num_frames: 5,
        num_drops: drops,
        last_frame_delay,
        audio_delay: u32::MAX,
    }
}

#[tokio::test]
async fn reports_of_late_frames_cut_the_target_and_clean_ones_raise_it() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (_video, create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    let (id, unique_id) = (create.id, viewer.unique_ids[&create.id]);
    let cap = video_cap();
    assert_eq!(stream_target(&server), cap);

    // Clean reports at the cap leave it there.
    let clean = report_on(id, unique_id, 0, 60);
    for _ in 0..10 {
        client
            .send(display_client::STREAM_REPORT, &body(&clean))
            .await;
        viewer
            .read_for(&mut client, Duration::from_millis(250))
            .await;
        assert_eq!(stream_target(&server), cap);
    }

    // Late frames halve it, down to the floor, though the last frame was
    // in time.
    let late = report_on(id, unique_id, 2, 30);
    for expected in [cap / 2, u64::from(MIN_STREAM_BITRATE_BPS)] {
        client
            .send(display_client::STREAM_REPORT, &body(&late))
            .await;
        viewer
            .read_until(&mut client, "the target falls", |_| {
                stream_target(&server) == expected
            })
            .await;
    }

    // Two seconds of clean reports raise it by a tenth, and not before.
    let floor = u64::from(MIN_STREAM_BITRATE_BPS);
    let first = Instant::now();
    let raised = loop {
        client
            .send(display_client::STREAM_REPORT, &body(&clean))
            .await;
        viewer
            .read_for(&mut client, Duration::from_millis(100))
            .await;
        if stream_target(&server) != floor {
            break first.elapsed();
        }
        assert!(first.elapsed() < support::WAIT, "it never rose");
    };
    assert!(
        raised >= CLEAN_PERIOD - Duration::from_millis(100),
        "it rose after {raised:?}"
    );
    assert_eq!(stream_target(&server), floor * 11 / 10);
}

#[tokio::test]
async fn a_client_that_stops_acknowledging_cuts_the_target() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (_video, _create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    let cap = video_cap();
    assert_eq!(stream_target(&server), cap);

    // The ack window fills, and frames due meanwhile are server drops.
    viewer.acking = false;
    viewer.read_for(&mut client, Duration::from_secs(1)).await;
    wait_until("the target falls", || stream_target(&server) < cap).await;
    let stats = server.handle.session().unwrap().stats();
    assert!(stats.stream_server_drops >= 2);
    assert_eq!(stats.streams_destroyed, 0, "the stream ended");
}

#[tokio::test]
async fn the_metrics_count_streams_and_carry_the_target() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let session = server.handle.session().unwrap();
    let before = session.stats();
    let (video, _create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;

    let during = session.stats().since(&before);
    assert_eq!((during.streams_created, during.streams_destroyed), (1, 0));
    assert!(during.stream_frames > 0);
    assert!(during.stream_bytes > 0);
    assert!(during.stream_encode.count >= during.stream_frames);
    assert_eq!(during.stream_target_bps, video_cap());
    let line = during.summary(Duration::from_secs(1));
    assert!(
        line.contains("streams created=1 destroyed=0, stream frames ")
            && line.contains("stream target 1000 kbit/s"),
        "{line}"
    );

    drop(video);
    viewer
        .read_until(&mut client, "the stream ends", |viewer| {
            !viewer.destroyed.is_empty()
        })
        .await;
    let after = session.stats().since(&before);
    assert_eq!((after.streams_created, after.streams_destroyed), (1, 1));
    // Every frame counted was sent before the STREAM_DESTROY, and so has
    // been read.
    assert_eq!(after.stream_frames, viewer.frames as u64);
    assert_eq!(after.stream_target_bps, 0);
    let line = after.summary(Duration::from_secs(1));
    assert!(line.contains("stream target -"), "{line}");
}

/// What andris warns once when captures start to fail.
const CAPTURES_FAILING: &str = "capturing the desktop failed; skipping frames until it works";

/// A capture that fails is a platform's failure, not the stream's: as
/// for draws, its frame is skipped and its damage kept, and capturing
/// resumes once it works. So the stream lives on, with one warning about
/// the captures and none about streams, and its area is still repaired
/// exactly when the video ends.
#[tokio::test(flavor = "current_thread")]
async fn captures_that_fail_while_streaming_skip_frames_and_the_stream_carries_on() {
    let (logs, _guard) = Captured::start(tracing::Level::WARN);
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (video, create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;

    // Three captures in a row fail: under a second of them, with the
    // retry gap, so the stream does not go still meanwhile.
    server.platform.fail_captures(3).unwrap();
    let frames = viewer.frames;
    viewer
        .read_until(&mut client, "frames flow again", |viewer| {
            viewer.frames >= frames + 3
        })
        .await;
    assert_eq!(viewer.created.len(), 1);
    assert!(viewer.destroyed.is_empty(), "the stream ended");
    assert_eq!(viewer.last_frame.as_ref().unwrap().base.id, create.id);

    drop(video);
    viewer
        .read_until(&mut client, "the stream ends", |viewer| {
            !viewer.destroyed.is_empty()
        })
        .await;
    viewer.read_for(&mut client, REPAIRED_WITHIN).await;
    viewer
        .screen
        .assert_shows(&server.platform.desktop().unwrap());

    let text = logs.text();
    assert_eq!(text.matches(CAPTURES_FAILING).count(), 1, "{text}");
    assert!(!text.contains(STREAMS_OFF), "{text}");
}

#[tokio::test]
async fn a_stream_report_too_short_closes_the_channel() {
    let mut report = Vec::new();
    stream_report(0, 0).write(&mut report);
    assert_eq!(report.len(), StreamReport::SIZE);
    for length in [0, StreamReport::SIZE - 1] {
        let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
        Viewer::start(&server, &mut client).await;
        client
            .send(display_client::STREAM_REPORT, &report[..length])
            .await;
        client.expect_closed().await;
    }
}

/// SPICE lets a later protocol add fields to a message, so a report
/// longer than andris knows is read for the fields it does know.
#[tokio::test]
async fn a_stream_report_with_bytes_to_spare_is_read() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;
    let (_video, create) = stream_video(&server, &mut client, &mut viewer, VIDEO).await;
    let mut report = body(&report_on(create.id, viewer.unique_ids[&create.id], 2, 30));
    report.extend_from_slice(&[0xAB; 8]);
    client.send(display_client::STREAM_REPORT, &report).await;
    let cap = video_cap();
    viewer
        .read_until(&mut client, "the target falls", |_| {
            stream_target(&server) == cap / 2
        })
        .await;
}

#[tokio::test]
async fn a_resize_with_no_stream_ever_sends_no_destroy_all() {
    let (server, _main, mut client) = open_wide(capabilities::DEFAULT_DISPLAY).await;
    Viewer::start(&server, &mut client).await;
    let size = Size {
        width: 640,
        height: 448,
    };
    server.platform.set_size(size).unwrap();
    // SURFACE_DESTROY comes first, with nothing before it.
    expect_replaced_surface(&server, &mut client, size).await;
}

/// A video up to the odd right and bottom edges of a desktop whose size
/// is not a multiple of 64. Its stream is whole tiles, even-sided, and
/// leaves out the last few pixels of each edge, which go as draws; no
/// draw lands inside the stream while it lives, and the whole area is
/// exact once it ends.
#[tokio::test]
async fn a_video_at_odd_edges_streams_whole_tiles_and_is_drawn_beside() {
    // The last tile column is 3 pixels wide, and the last row 3 tall.
    let size = Size {
        width: 643,
        height: 387,
    };
    let display = DisplayOptions {
        frame_interval: INTERVAL,
        ..display_options()
    };
    let server = TestServer::start_sized(size, display).await;
    let (server, _main, mut client) = open_display_on(server, capabilities::DEFAULT_DISPLAY).await;
    let mut viewer = Viewer::start(&server, &mut client).await;

    let video_area = Rectangle::new(384, 192, 259, 195);
    let (video, create) = stream_video(&server, &mut client, &mut viewer, video_area).await;
    let stream = Rectangle::new(384, 192, 256, 192);
    assert_eq!(create.dest, wire_rect(stream));
    assert_eq!((create.stream_width % 2, create.stream_height % 2), (0, 0));
    let edge = Rectangle::new(640, 192, 3, 195);
    let beside = |viewer: &Viewer| {
        viewer
            .draws_while_streaming
            .iter()
            .filter(|bbox| overlaps(bbox, edge))
            .count()
    };
    viewer
        .read_until(&mut client, "ten draws of the edge column", |viewer| {
            beside(viewer) >= 10
        })
        .await;
    drop(video);
    viewer
        .read_until(&mut client, "the stream ends", |viewer| {
            !viewer.destroyed.is_empty()
        })
        .await;

    let inside: Vec<&Rect> = viewer
        .draws_while_streaming
        .iter()
        .filter(|bbox| overlaps(bbox, stream))
        .collect();
    assert!(inside.is_empty(), "draws inside the stream: {inside:?}");

    viewer.read_for(&mut client, REPAIRED_WITHIN).await;
    viewer
        .screen
        .assert_shows(&server.platform.desktop().unwrap());
}
