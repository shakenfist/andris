//! A display channel's video stream: an area of the surface sent as
//! H.264 frames rather than as draws.
//!
//! # One stream at a time
//!
//! [`Streams`] holds a channel's stream state: whether the client may be
//! sent streams at all, the one live [`Stream`] if there is one, and the
//! id the next stream takes. Ids cycle through 0 to [`STREAM_IDS`] - 1:
//! a new stream takes another id than the one before it, so a late
//! message about that one is not taken for the new one, and ids stay
//! small, since clients index their streams by id. A client may be sent
//! streams only when it has both the
//! `MULTI_CODEC` and `CODEC_H264` display capabilities, since H.264 is
//! the only codec andris encodes. Its `PREFERRED_VIDEO_CODEC_TYPE` order
//! therefore changes nothing.
//!
//! # A stream's messages
//!
//! - `STREAM_CREATE` draws the stream on the primary surface, top down,
//!   as H.264, with the stream, its source and its destination all the
//!   rectangle's size, and no clip.
//! - `STREAM_ACTIVATE_REPORT` follows it, for a client with the
//!   `STREAM_REPORT` capability, asking for a report every
//!   [`REPORT_WINDOW`] frames or [`REPORT_TIMEOUT`], with a random
//!   `unique_id` that tells this stream's reports from those of an
//!   earlier stream with the same id.
//! - `STREAM_DATA` carries one frame, exactly the rectangle's size,
//!   stamped with the multimedia time [`multi_media_time`] gives it. A
//!   frame the encoder skips to hold its bitrate sends nothing.
//! - `STREAM_DESTROY` ends the stream; `STREAM_DESTROY_ALL` ends every
//!   stream, before the surface they are drawn on is destroyed.
//!
//! # Reports
//!
//! A report counts only for the live stream, and only with its
//! `unique_id`; any other is ignored. The exception is the report that
//! says the client cannot decode the stream (`num_frames` 0 and
//! `num_drops` `u32::MAX`), which counts whatever its `unique_id`:
//! spice-gtk sends it zeroed, for a stream it failed to create or
//! decode. It ends the stream, and the channel then sends no more
//! streams, as it does after the encoder fails.
//!
//! # Its tiles
//!
//! A stream's rectangle is tile-aligned but where the surface's edge
//! clips it, or rounding it to even dimensions leaves a row or column
//! out. [`Stream::covered`] holds the tiles it covers wholly: damage to
//! those goes into the stream, and damage anywhere else, a partly
//! covered tile included, is drawn as usual. [`Stream::touched`] holds
//! every tile it touches, which its frames leave lossy.
//!
//! # Its end
//!
//! A stream that has not changed for [`STREAM_QUIET`] ends. Until then,
//! its tiles are not due for the lossless refresh: they are due once it
//! has ended, which then repairs the area.
//!
//! This module knows nothing of sockets: the display channel sends what
//! it builds, and runs [`Stream::encode`] off the async threads. Which
//! area streams is the heat map's to find.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use shakenfist_spice_compression::{Bgrx, H264Config, H264Encoder, H264Frame, even_dimensions};
use shakenfist_spice_protocol::constants::stream_flags;
use shakenfist_spice_protocol::messages::{
    Clip, StreamActivateReport, StreamCreate, StreamData, StreamDataHeader, StreamDestroy,
    StreamReport,
};
use tokio::time::Instant;

use super::encode::{SURFACE_ID, wire_rect};
use crate::platform::{Frame, Rectangle, Region, Size};

/// `SPICE_VIDEO_CODEC_TYPE_H264` (spice-protocol enums.h), which ryll's
/// protocol crate does not name; its compression crate does, but only
/// beside its decoders.
pub const CODEC_TYPE_H264: u8 = 3;

/// How many stream ids there are: ids cycle through 0 to 63.
pub const STREAM_IDS: u32 = 64;

/// How far ahead of its capture a frame is stamped.
///
/// A client judges a frame late when its multimedia time is behind the
/// client's clock, which is the server's clock as `MAIN_INIT` gave it
/// plus the time since: ryll then counts it as dropped in its reports,
/// and spice-gtk presents a frame at its time, dropping a late MJPEG
/// frame before decoding. A frame stamped at its capture arrives late by
/// however long it took to encode and send, so every frame would count
/// as late. Stamped this far ahead, a frame is late only when the
/// pipeline took longer than this, and a report's `last_frame_delay` is
/// the headroom left, which is what rate control needs to see.
///
/// It costs a client that presents frames at their time, as spice-gtk
/// does, up to this much latency. Ryll paints frames as they arrive, and
/// pays nothing. No `MAIN_MULTI_MEDIA_TIME` is sent to correct the
/// client's clock: both clocks are monotonic, and they drift apart over a
/// session by far less than this.
pub const STREAM_LATENCY_BUDGET: Duration = Duration::from_millis(80);

/// How long a stream may go without a change before it ends, and its
/// area is sent again losslessly: spice-server's timeout. Long enough to
/// ride out a video's still scene or a short stall in its player, short
/// enough that a paused video is sharp again in about a second.
pub const STREAM_QUIET: Duration = Duration::from_secs(1);

/// How many frames a client's report covers at most.
pub const REPORT_WINDOW: u32 = 5;

/// The longest a client waits between reports, while frames arrive.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(1);

/// The multimedia time of a frame captured `captured_at` into the
/// session: the milliseconds since the session began, as `MAIN_INIT`
/// counts them, plus [`STREAM_LATENCY_BUDGET`]. Wrapping, as the field
/// is a `u32`.
pub fn multi_media_time(captured_at: Duration) -> u32 {
    (captured_at + STREAM_LATENCY_BUDGET).as_millis() as u32
}

/// The frame rate a stream is encoded for: one frame every
/// `frame_interval`, rounded to whole frames a second, and at least one.
pub fn frames_per_second(frame_interval: Duration) -> u32 {
    let millis = frame_interval.as_millis().max(1);
    u32::try_from((1000 + millis / 2) / millis)
        .unwrap_or(u32::MAX)
        .max(1)
}

/// What the client's display capabilities allow of streams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamCaps {
    /// Whether the client can decode H.264 streams: it has both
    /// `MULTI_CODEC` and `CODEC_H264`.
    pub h264: bool,
    /// Whether the client sends `STREAM_REPORT`s when asked.
    pub reports: bool,
}

/// What a client's `STREAM_REPORT` meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportVerdict {
    /// It is the live stream's latest report, now recorded.
    Recorded,
    /// The client cannot decode the live stream.
    CannotDecode,
    /// No stream lives.
    NoStream,
    /// It names another stream than the live one.
    OtherStream,
    /// It names the live stream's id with another `unique_id`: it is
    /// from an earlier stream with the same id, or reports were never
    /// asked for.
    OtherUniqueId,
}

/// A display channel's streams: whether it may send them, the live one,
/// and which id is next.
#[derive(Debug)]
pub struct Streams {
    caps: StreamCaps,
    frame_rate: u32,
    next_id: u32,
    live: Option<Stream>,
    /// Set for good when the client cannot decode a stream, or the
    /// encoder fails.
    disabled: bool,
    /// Whether any stream has been created on this channel.
    created_any: bool,
}

impl Streams {
    /// No stream yet, for a client with `caps`, encoding for one frame
    /// every `frame_interval`.
    pub fn new(caps: StreamCaps, frame_interval: Duration) -> Self {
        Self {
            caps,
            frame_rate: frames_per_second(frame_interval),
            next_id: 0,
            live: None,
            disabled: false,
            created_any: false,
        }
    }

    /// Whether streams may be sent at all: the client can decode H.264,
    /// and streams have not been turned off.
    pub fn enabled(&self) -> bool {
        self.caps.h264 && !self.disabled
    }

    /// Whether a stream may start: streams are [`Streams::enabled`], and
    /// none is live.
    pub fn may_start(&self) -> bool {
        self.enabled() && self.live.is_none()
    }

    /// Whether streams have been turned off for this channel.
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// The live stream.
    pub fn live(&self) -> Option<&Stream> {
        self.live.as_ref()
    }

    /// The live stream, to change.
    pub fn live_mut(&mut self) -> Option<&mut Stream> {
        self.live.as_mut()
    }

    /// Start a stream over `rect` on a surface of `size` at `now`, if
    /// [`Streams::may_start`]. The rectangle is clipped to the surface and
    /// rounded down to even dimensions, as H.264 needs. Returns the new
    /// stream, or `None` if none may start or nothing of `rect` is left.
    /// Fails if the encoder cannot be built; the caller then turns
    /// streams off with [`Streams::disable`].
    pub fn start(&mut self, rect: Rectangle, size: Size, now: Instant) -> Result<Option<&Stream>> {
        if !self.may_start() {
            return Ok(None);
        }
        let Some(clipped) = rect.clip(size) else {
            return Ok(None);
        };
        let (width, height) = even_dimensions(clipped.width, clipped.height);
        let rect = Rectangle::new(clipped.x, clipped.y, width, height);
        if rect.is_empty() {
            return Ok(None);
        }
        let config = H264Config::new(width, height, self.frame_rate);
        let target_bps = config.bitrate_bps;
        let encoder = H264Encoder::new(config)
            .with_context(|| format!("creating an H.264 encoder for {rect}"))?;
        let id = self.next_id;
        self.next_id = (id + 1) % STREAM_IDS;
        self.created_any = true;
        Ok(Some(&*self.live.insert(Stream {
            id,
            rect,
            encoder: Box::new(encoder),
            target_bps,
            unique_id: self.caps.reports.then(rand::random),
            last_report: None,
            last_change: now,
            server_drops: 0,
        })))
    }

    /// Take the live stream out, to encode a frame of it off the async
    /// threads. [`Streams::put_back`] returns it; until then no stream
    /// lives.
    pub fn take(&mut self) -> Option<Stream> {
        self.live.take()
    }

    /// Return a stream [`Streams::take`] took out.
    pub fn put_back(&mut self, stream: Stream) {
        debug_assert!(self.live.is_none(), "a stream was put back over another");
        self.live = Some(stream);
    }

    /// End the live stream, returning it for its `STREAM_DESTROY`.
    pub fn end(&mut self) -> Option<Stream> {
        self.live.take()
    }

    /// End every stream, as the surface they are drawn on goes. Returns
    /// whether any stream was ever created on this channel, and so
    /// whether the client needs a `STREAM_DESTROY_ALL`.
    pub fn end_all(&mut self) -> bool {
        self.live = None;
        self.created_any
    }

    /// Turn streams off for the rest of this channel. Returns whether
    /// they were on until now. Any live stream must be ended too.
    pub fn disable(&mut self) -> bool {
        !std::mem::replace(&mut self.disabled, true)
    }

    /// Judge a client's report, and record it if it is the live stream's.
    pub fn report(&mut self, report: StreamReport) -> ReportVerdict {
        let Some(stream) = self.live.as_mut() else {
            return ReportVerdict::NoStream;
        };
        if report.stream_id != stream.id {
            return ReportVerdict::OtherStream;
        }
        if report.num_frames == 0 && report.num_drops == u32::MAX {
            return ReportVerdict::CannotDecode;
        }
        if stream.unique_id != Some(report.unique_id) {
            return ReportVerdict::OtherUniqueId;
        }
        stream.last_report = Some(report);
        ReportVerdict::Recorded
    }
}

/// One live stream: its id, its rectangle on the primary surface, its
/// encoder, and what its client last reported.
pub struct Stream {
    id: u32,
    /// Even-sized, and within the surface.
    rect: Rectangle,
    /// Boxed, because it is several kilobytes, and a stream moves in and
    /// out of the display channel's futures, whose every copy a debug
    /// build keeps on the stack.
    encoder: Box<H264Encoder>,
    /// The bitrate the stream aims for, in bits a second. It starts at
    /// the encoder's own.
    target_bps: u32,
    /// The `unique_id` its reports carry, if they were asked for.
    unique_id: Option<u32>,
    last_report: Option<StreamReport>,
    /// When a frame last changed it, or when it started.
    last_change: Instant,
    /// How many frames were due while the client's ack window was full,
    /// and so were never captured.
    server_drops: u64,
}

impl std::fmt::Debug for Stream {
    // The encoder has nothing worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("id", &self.id)
            .field("rect", &self.rect)
            .field("target_bps", &self.target_bps)
            .field("unique_id", &self.unique_id)
            .field("last_report", &self.last_report)
            .field("last_change", &self.last_change)
            .field("server_drops", &self.server_drops)
            .finish_non_exhaustive()
    }
}

impl Stream {
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The part of the surface the stream draws.
    pub fn rect(&self) -> Rectangle {
        self.rect
    }

    /// The bitrate the stream aims for, in bits a second.
    pub fn target_bps(&self) -> u32 {
        self.target_bps
    }

    /// The `unique_id` its reports carry, if they were asked for.
    pub fn unique_id(&self) -> Option<u32> {
        self.unique_id
    }

    /// The client's last report for this stream.
    pub fn last_report(&self) -> Option<&StreamReport> {
        self.last_report.as_ref()
    }

    /// The tiles of a surface of `size` the stream covers wholly: damage
    /// to them goes into the stream rather than into draws.
    pub fn covered(&self, size: Size) -> Region {
        let mut covered = Region::empty(size);
        for (col, row) in self.rect.tiles() {
            if self.rect.contains(&covered.tile_rect(col, row)) {
                covered.insert_tile(col, row);
            }
        }
        covered
    }

    /// Every tile of a surface of `size` the stream touches, wholly or
    /// in part: its frames leave them all lossy.
    pub fn touched(&self, size: Size) -> Region {
        let mut touched = Region::empty(size);
        touched.add(self.rect);
        touched
    }

    /// Record that a frame sent at `now` changed the stream.
    pub fn changed(&mut self, now: Instant) {
        self.last_change = now;
    }

    /// When the stream ends if nothing changes it first.
    pub fn quiet_at(&self) -> Instant {
        self.last_change + STREAM_QUIET
    }

    /// Count a frame that was due while the client's ack window was
    /// full, and so was never captured.
    pub fn server_drop(&mut self) {
        self.server_drops = self.server_drops.saturating_add(1);
    }

    /// How many frames were due while the client's ack window was full.
    pub fn server_drops(&self) -> u64 {
        self.server_drops
    }

    /// The stream's `STREAM_CREATE`.
    pub fn create(&self) -> Result<StreamCreate> {
        let (width, height) = (self.rect.width, self.rect.height);
        Ok(StreamCreate {
            surface_id: SURFACE_ID,
            id: self.id,
            flags: stream_flags::TOP_DOWN,
            codec_type: CODEC_TYPE_H264,
            stamp: 0,
            stream_width: width,
            stream_height: height,
            src_width: width,
            src_height: height,
            dest: wire_rect(self.rect)?,
            clip: Clip::none(),
        })
    }

    /// The `STREAM_ACTIVATE_REPORT` that follows `STREAM_CREATE`, if
    /// reports were asked for.
    pub fn activate_report(&self) -> Option<StreamActivateReport> {
        Some(StreamActivateReport {
            stream_id: self.id,
            unique_id: self.unique_id?,
            max_window_size: REPORT_WINDOW,
            timeout_ms: REPORT_TIMEOUT.as_millis() as u32,
        })
    }

    /// The `STREAM_DATA` for `frame`, captured `captured_at` into the
    /// session.
    pub fn data(&self, frame: H264Frame, captured_at: Duration) -> StreamData {
        StreamData {
            base: StreamDataHeader {
                id: self.id,
                multi_media_time: multi_media_time(captured_at),
            },
            data: frame.data,
        }
    }

    /// The stream's `STREAM_DESTROY`.
    pub fn destroy(&self) -> StreamDestroy {
        StreamDestroy { id: self.id }
    }

    /// Encode `pixels`, a capture of the stream's rectangle. Returns
    /// `None` when the encoder skipped the frame to hold its bitrate, and
    /// nothing is to be sent. CPU-bound, so the display channel runs it
    /// on the blocking pool. Fails unless `pixels` is exactly the
    /// rectangle's size, or if the encoder fails.
    pub fn encode(&mut self, pixels: &Frame) -> Result<Option<H264Frame>> {
        ensure!(
            pixels.size() == self.rect.size(),
            "a {} capture is not the {} stream's size",
            pixels.size(),
            self.rect
        );
        let image = Bgrx::new(
            pixels.data(),
            pixels.width(),
            pixels.height(),
            pixels.stride(),
        )
        .context("a capture the encoder cannot read")?;
        self.encoder.encode(&image)
    }
}

// The display channel moves a stream to the blocking pool to encode it.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Stream>();
};

#[cfg(test)]
mod tests {
    use shakenfist_spice_compression::{JpegDecoderRsDecoder, SPICE_VIDEO_CODEC_TYPE_H264};
    use shakenfist_spice_protocol::constants::clip_type;
    use shakenfist_spice_protocol::messages::Rect;
    use std::sync::Arc;

    use super::*;

    const SIZE: Size = Size {
        width: 256,
        height: 192,
    };

    const BOTH: StreamCaps = StreamCaps {
        h264: true,
        reports: true,
    };

    fn streams(caps: StreamCaps) -> Streams {
        Streams::new(caps, Duration::from_millis(33))
    }

    fn report(stream_id: u32, unique_id: u32) -> StreamReport {
        StreamReport {
            stream_id,
            unique_id,
            start_frame_mm_time: 100,
            end_frame_mm_time: 200,
            num_frames: 5,
            num_drops: 1,
            last_frame_delay: 40,
            audio_delay: u32::MAX,
        }
    }

    fn cannot_decode(stream_id: u32) -> StreamReport {
        StreamReport {
            num_frames: 0,
            num_drops: u32::MAX,
            ..report(stream_id, 0)
        }
    }

    #[test]
    fn the_h264_codec_type_is_the_one_ryll_decodes() {
        assert_eq!(CODEC_TYPE_H264, SPICE_VIDEO_CODEC_TYPE_H264);
    }

    #[test]
    fn frames_are_stamped_ahead_by_the_budget_and_wrap() {
        assert_eq!(multi_media_time(Duration::ZERO), 80);
        assert_eq!(multi_media_time(Duration::from_micros(1_234_567)), 1314);
        let wrap = Duration::from_millis(u64::from(u32::MAX) + 1);
        assert_eq!(multi_media_time(wrap), 80);
    }

    #[test]
    fn the_frame_rate_follows_the_frame_interval() {
        assert_eq!(frames_per_second(Duration::from_millis(33)), 30);
        assert_eq!(frames_per_second(Duration::from_millis(20)), 50);
        assert_eq!(frames_per_second(Duration::from_millis(40)), 25);
        assert_eq!(frames_per_second(Duration::from_secs(5)), 1);
        assert_eq!(frames_per_second(Duration::ZERO), 1000);
    }

    #[test]
    fn create_draws_the_rectangle_top_down_as_h264() {
        let mut streams = streams(BOTH);
        let rect = Rectangle::new(64, 32, 128, 96);
        let stream = streams.start(rect, SIZE, Instant::now()).unwrap().unwrap();
        let create = stream.create().unwrap();
        assert_eq!(
            create,
            StreamCreate {
                surface_id: 0,
                id: 0,
                flags: stream_flags::TOP_DOWN,
                codec_type: 3,
                stamp: 0,
                stream_width: 128,
                stream_height: 96,
                src_width: 128,
                src_height: 96,
                dest: Rect {
                    top: 32,
                    left: 64,
                    bottom: 128,
                    right: 192,
                },
                clip: Clip::none(),
            }
        );
        assert_eq!(create.clip.clip_type, clip_type::NONE);
        assert_eq!(stream.destroy(), StreamDestroy { id: 0 });
    }

    #[test]
    fn reports_are_activated_only_for_a_client_that_sends_them() {
        let mut with = streams(BOTH);
        let stream = with
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        let unique_id = stream.unique_id().unwrap();
        assert_eq!(
            stream.activate_report(),
            Some(StreamActivateReport {
                stream_id: 0,
                unique_id,
                max_window_size: 5,
                timeout_ms: 1000,
            })
        );

        let mut without = streams(StreamCaps {
            h264: true,
            reports: false,
        });
        let stream = without
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        assert_eq!(stream.unique_id(), None);
        assert_eq!(stream.activate_report(), None);
    }

    #[test]
    fn data_carries_the_frame_and_its_time() {
        let mut streams = streams(BOTH);
        let stream = streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        let frame = H264Frame {
            data: vec![0, 0, 0, 1, 0x65],
            keyframe: true,
        };
        let data = stream.data(frame, Duration::from_millis(1000));
        assert_eq!(
            data,
            StreamData {
                base: StreamDataHeader {
                    id: 0,
                    multi_media_time: 1080,
                },
                data: vec![0, 0, 0, 1, 0x65],
            }
        );
    }

    #[test]
    fn the_rectangle_is_clipped_and_rounded_down_to_even() {
        let mut streams = streams(BOTH);
        let stream = streams
            .start(Rectangle::new(201, 99, 300, 99), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        // Clipped to 55 x 93, then rounded down.
        assert_eq!(stream.rect(), Rectangle::new(201, 99, 54, 92));
        streams.end();

        // Outside the surface, or a pixel wide: nothing starts.
        assert!(
            streams
                .start(Rectangle::new(300, 0, 64, 64), SIZE, Instant::now())
                .unwrap()
                .is_none()
        );
        assert!(
            streams
                .start(Rectangle::new(0, 0, 1, 64), SIZE, Instant::now())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ids_cycle_with_one_stream_live_at_a_time() {
        let mut streams = streams(BOTH);
        let rect = Rectangle::new(0, 0, 64, 64);
        for expected in (0..STREAM_IDS).chain(0..2) {
            let id = streams
                .start(rect, SIZE, Instant::now())
                .unwrap()
                .unwrap()
                .id();
            assert_eq!(id, expected);
            // A second stream waits for the first to end.
            assert!(!streams.may_start());
            assert!(streams.start(rect, SIZE, Instant::now()).unwrap().is_none());
            assert_eq!(streams.end().unwrap().id(), expected);
        }
    }

    #[test]
    fn no_stream_starts_without_h264() {
        let mut streams = streams(StreamCaps {
            h264: false,
            reports: true,
        });
        assert!(!streams.may_start());
        assert!(
            streams
                .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
                .unwrap()
                .is_none()
        );
        assert!(!streams.end_all(), "no stream was ever created");
    }

    #[test]
    fn disabled_streams_stay_off_and_say_so_once() {
        let mut streams = streams(BOTH);
        assert!(streams.disable());
        assert!(!streams.disable());
        assert!(streams.is_disabled());
        assert!(
            streams
                .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn end_all_says_whether_a_stream_was_ever_created() {
        let mut streams = streams(BOTH);
        assert!(!streams.end_all());
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        assert!(streams.end_all());
        assert!(streams.live().is_none());
        // Still true with no stream live: the client may hold one.
        assert!(streams.end_all());
    }

    #[test]
    fn reports_count_only_for_the_live_stream_and_its_unique_id() {
        let mut streams = streams(BOTH);
        assert_eq!(streams.report(report(0, 0)), ReportVerdict::NoStream);
        let unique_id = streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap()
            .unique_id()
            .unwrap();

        assert_eq!(
            streams.report(report(1, unique_id)),
            ReportVerdict::OtherStream
        );
        assert_eq!(
            streams.report(report(0, unique_id.wrapping_add(1))),
            ReportVerdict::OtherUniqueId
        );
        assert_eq!(streams.live().unwrap().last_report(), None);

        assert_eq!(
            streams.report(report(0, unique_id)),
            ReportVerdict::Recorded
        );
        assert_eq!(
            streams.live().unwrap().last_report(),
            Some(&report(0, unique_id))
        );
    }

    #[test]
    fn cannot_decode_counts_whatever_its_unique_id_but_not_for_another_stream() {
        let mut streams = streams(BOTH);
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        assert_eq!(streams.report(cannot_decode(1)), ReportVerdict::OtherStream);
        // spice-gtk zeroes the unique_id.
        assert_eq!(
            streams.report(cannot_decode(0)),
            ReportVerdict::CannotDecode
        );
        // Frames, however many dropped, are not the special case.
        let dropped = StreamReport {
            num_frames: 1,
            ..cannot_decode(0)
        };
        assert_eq!(streams.report(dropped), ReportVerdict::OtherUniqueId);
    }

    #[test]
    fn without_reports_asked_for_only_cannot_decode_counts() {
        let mut streams = streams(StreamCaps {
            h264: true,
            reports: false,
        });
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        assert_eq!(streams.report(report(0, 0)), ReportVerdict::OtherUniqueId);
        assert_eq!(
            streams.report(cannot_decode(0)),
            ReportVerdict::CannotDecode
        );
    }

    /// A BGRX frame of `size` whose rows are padded, as captures are.
    fn padded_frame(size: Size, shade: u8) -> Frame {
        let stride = size.width as usize * 4 + 12;
        let mut data = vec![0xAA; stride * size.height as usize];
        for y in 0..size.height as usize {
            for x in 0..size.width as usize {
                let at = y * stride + x * 4;
                data[at..at + 4].copy_from_slice(&[shade, (x * 2) as u8, (y * 2) as u8, 0]);
            }
        }
        Frame::new(size.width, size.height, stride, data).unwrap()
    }

    #[test]
    fn a_padded_capture_encodes_to_a_frame_ryll_decodes() {
        let mut streams = streams(BOTH);
        let rect = Rectangle::new(64, 64, 96, 64);
        streams.start(rect, SIZE, Instant::now()).unwrap().unwrap();
        let mut stream = streams.take().unwrap();
        let pixels = padded_frame(rect.size(), 0x40);
        let frame = stream.encode(&pixels).unwrap().expect("the first frame");
        assert!(frame.keyframe);

        let mut decoder = shakenfist_spice_compression::for_stream(
            CODEC_TYPE_H264,
            Arc::new(JpegDecoderRsDecoder::new()),
        )
        .unwrap();
        let decoded = decoder.decode(&frame.data).unwrap().unwrap();
        assert_eq!((decoded.width, decoded.height), (96, 64));
        streams.put_back(stream);
        assert_eq!(streams.live().unwrap().rect(), rect);
    }

    #[test]
    fn a_capture_of_another_size_is_an_error() {
        let mut streams = streams(BOTH);
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        let mut stream = streams.take().unwrap();
        let err = stream
            .encode(&padded_frame(
                Size {
                    width: 66,
                    height: 64,
                },
                0,
            ))
            .unwrap_err();
        assert!(err.to_string().contains("is not the"), "{err}");
    }

    #[test]
    fn covered_tiles_are_the_whole_ones_and_touched_tiles_all_of_them() {
        // A surface 301 x 200: the last tile column is 45 pixels wide and
        // the last row 8 rows tall.
        let size = Size {
            width: 301,
            height: 200,
        };
        let mut streams = streams(BOTH);
        let stream = streams
            .start(Rectangle::new(192, 128, 200, 100), size, Instant::now())
            .unwrap()
            .unwrap();
        // Clipped to 109 x 72, then rounded to 108 x 72: the surface's
        // last column stays out of the stream, and so does the last tile
        // column, which holds it.
        assert_eq!(stream.rect(), Rectangle::new(192, 128, 108, 72));
        let mut covered = Region::empty(size);
        covered.add(Rectangle::new(192, 128, 64, 72));
        assert_eq!(stream.covered(size), covered);
        let mut touched = Region::empty(size);
        touched.add(Rectangle::new(192, 128, 109, 72));
        assert_eq!(stream.touched(size), touched);

        // A tile-aligned stream covers every tile it touches.
        streams.end();
        let rect = Rectangle::new(64, 0, 128, 128);
        let stream = streams.start(rect, SIZE, Instant::now()).unwrap().unwrap();
        assert_eq!(stream.covered(SIZE), stream.touched(SIZE));
        assert_eq!(stream.covered(SIZE).tile_count(), 4);
    }

    #[test]
    fn a_stream_goes_quiet_a_second_after_its_last_change() {
        let start = Instant::now();
        let mut streams = streams(BOTH);
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, start)
            .unwrap()
            .unwrap();
        let mut stream = streams.take().unwrap();
        assert_eq!(stream.quiet_at(), start + STREAM_QUIET);
        let later = start + Duration::from_millis(700);
        stream.changed(later);
        assert_eq!(stream.quiet_at(), later + STREAM_QUIET);

        assert_eq!(stream.server_drops(), 0);
        stream.server_drop();
        stream.server_drop();
        assert_eq!(stream.server_drops(), 2);
    }

    #[test]
    fn enabled_until_disabled_and_only_with_h264() {
        let mut streams = streams(BOTH);
        assert!(streams.enabled());
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        assert!(streams.enabled() && !streams.may_start());
        streams.disable();
        assert!(!streams.enabled());
        assert!(!self::streams(StreamCaps::default()).enabled());
    }
}
