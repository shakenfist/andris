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
//! `num_drops` `u32::MAX`), which also counts with a `unique_id` of 0:
//! spice-gtk sends it zeroed, for a stream it failed to create or
//! decode. It ends the stream, and the channel then sends no more
//! streams, as it does after the encoder fails. With any other
//! `unique_id` it is from an earlier stream with the same id (ids come
//! round every [`STREAM_IDS`] streams), and is ignored like any other
//! report for it. A zeroed report from an earlier stream with the same
//! id cannot be told apart, but it would have to arrive after 64 more
//! streams had started.
//!
//! # Its tiles
//!
//! A stream's rectangle is whole tiles, each clipped to the surface:
//! [`Streams::start`] shrinks what it is given to the tiles it covers
//! wholly, and where that leaves a side of odd length, which H.264
//! cannot encode, it leaves out the last tile on that side too, which is
//! then less than a tile at an odd surface edge. So no tile is part
//! stream and part draws, and one set of tiles, [`Stream::tiles`],
//! serves for everything: damage to them goes into the stream and
//! nowhere else, its frames leave them lossy until it ends, and they
//! are repaired and their heat forgotten when it does. Damage anywhere
//! else is drawn as usual.
//!
//! # Its bitrate
//!
//! A stream starts at [`target_bitrate_bps`] for its size and the
//! configured frame rate, which is also the most it may ever aim for.
//! [`RateControl`] then moves the target on the client's reports and on
//! andris's own server drops (frames due while the ack window was full):
//! down quickly when frames arrive late or the window fills, up slowly
//! while reports stay clean. Each change goes to the encoder at once.
//!
//! openh264's bitrate mode budgets each frame as the target divided by
//! the frame rate it was told, whatever the frames' times, and a stream
//! sends a frame only when its area changed, often less often than the
//! configured rate. So the stream also tracks the rate it actually
//! sends at, and keeps the encoder's near it ([`Stream::pace`]):
//! otherwise every frame gets too small a share, and the stream falls
//! far short of its target.
//!
//! The encoder is told the frames are video, not screen content. For
//! screen content openh264 encodes every frame at its lowest QP and
//! meets the target only by skipping frames, so a moving picture would
//! play at a few frames a second; video fits each frame to its share.
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

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use shakenfist_spice_compression::{
    Bgrx, H264Config, H264Content, H264Encoder, H264Frame, even_dimensions, target_bitrate_bps,
};
use shakenfist_spice_protocol::constants::stream_flags;
use shakenfist_spice_protocol::messages::{
    Clip, StreamActivateReport, StreamCreate, StreamData, StreamDataHeader, StreamDestroy,
    StreamReport,
};
use tokio::time::Instant;

use super::encode::{SURFACE_ID, wire_rect};
use crate::platform::{Frame, Rectangle, Region, Size, TILE};

/// `SPICE_VIDEO_CODEC_TYPE_H264` (spice-protocol enums.h), which ryll's
/// protocol crate does not name; its compression crate does, but only
/// beside its decoders.
pub const CODEC_TYPE_H264: u8 = 3;

/// The longest side of a stream: the longest side of the largest frame
/// openh264 encodes, which ryll's `H264Encoder` refuses to be built
/// beyond. An area larger than this is never streamed (see
/// [`fits_encoder`]), rather than streamed in part.
pub const MAX_STREAM_LONG_SIDE: u32 = 3840;

/// The longest a stream's shorter side may be: the shorter side of the
/// largest frame openh264 encodes, as for [`MAX_STREAM_LONG_SIDE`].
pub const MAX_STREAM_SHORT_SIDE: u32 = 2160;

/// The largest rectangle of whole tiles inside `rect` on a surface of
/// `size`, each tile clipped to the surface, with even sides as H.264
/// needs, or `None` if there is none. Its left and top edges are on a
/// tile boundary; its right and bottom edges are too, or are the
/// surface's. A side that would come out odd, which only happens where
/// it reaches an odd surface edge, loses its last tile, so the
/// rectangle never covers part of a tile.
pub fn whole_tiles(rect: Rectangle, size: Size) -> Option<Rectangle> {
    let clipped = rect.clip(size)?;
    // From `start` to `end` on an axis whose surface ends at `edge`.
    let span = |start: u32, end: u64, edge: u32| -> Option<(u32, u32)> {
        let first = start.div_ceil(TILE).checked_mul(TILE)?;
        let mut last = if end >= u64::from(edge) {
            edge
        } else {
            // end < edge, so it fits a u32.
            end as u32 / TILE * TILE
        };
        if last > first && (last - first) % 2 == 1 {
            last = (last - 1) / TILE * TILE;
        }
        if last > first {
            Some((first, last - first))
        } else {
            None
        }
    };
    let (x, width) = span(clipped.x, clipped.right(), size.width)?;
    let (y, height) = span(clipped.y, clipped.bottom(), size.height)?;
    Some(Rectangle::new(x, y, width, height))
}

/// Whether a `width` x `height` area, once rounded down to even sides,
/// is small enough for the encoder: at most [`MAX_STREAM_LONG_SIDE`] on
/// its longer side and [`MAX_STREAM_SHORT_SIDE`] on its shorter.
pub fn fits_encoder(width: u32, height: u32) -> bool {
    let (width, height) = even_dimensions(width, height);
    width.max(height) <= MAX_STREAM_LONG_SIDE && width.min(height) <= MAX_STREAM_SHORT_SIDE
}

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

/// The lowest bitrate rate control goes to, in bits a second. Below
/// this a desktop-sized stream is too blurred to be worth watching; a
/// link that cannot carry it is better served by the draws a client
/// without streams gets.
pub const MIN_STREAM_BITRATE_BPS: u32 = 256_000;

/// A report whose last frame arrived with less headroom than this before
/// its time, though in time, cuts the bitrate by a quarter: the pipeline
/// is close to using up [`STREAM_LATENCY_BUDGET`], and the next slower
/// frame would be late.
pub const LOW_HEADROOM: Duration = Duration::from_millis(20);

/// How long reports must stay clean, with no change in between, before
/// the bitrate rises by [`RAISE_PERCENT`]. Rising slowly and falling
/// fast keeps a stream from oscillating around what the link carries.
pub const CLEAN_PERIOD: Duration = Duration::from_secs(2);

/// How much a clean [`CLEAN_PERIOD`] raises the bitrate, in percent.
pub const RAISE_PERCENT: u32 = 10;

/// Two server drops this close together cut the bitrate by a quarter:
/// the client is acknowledging more slowly than the stream sends, even
/// if its reports have not said so yet. One alone may be a hiccup.
pub const SERVER_DROP_WINDOW: Duration = Duration::from_secs(1);

/// How far back [`Stream::pace`] counts frames to measure the rate a
/// stream sends at.
pub const FRAME_RATE_WINDOW: Duration = Duration::from_secs(1);

/// How far, in percent, the measured frame rate must move from the
/// encoder's before the encoder is told: a stream's rate wobbles from
/// one second to the next, and each change shifts every frame's budget.
/// Small, because the budget follows the rate: a 25 fps video in a
/// stream budgeted at the default 30 fps came to 84% of its target,
/// a gap a looser threshold (20%) left uncorrected.
pub const FRAME_RATE_HYSTERESIS_PERCENT: f32 = 10.0;

/// What may move a stream's bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateEvent {
    /// A client's `STREAM_REPORT` for the stream: how many frames it
    /// covers, how many of them were late, and the last one's headroom,
    /// in milliseconds before its time (negative if it was late).
    Report {
        frames: u32,
        drops: u32,
        last_frame_delay_ms: i32,
    },
    /// A frame was due while the client's ack window was full.
    ServerDrop,
}

impl From<&StreamReport> for RateEvent {
    fn from(report: &StreamReport) -> Self {
        Self::Report {
            frames: report.num_frames,
            drops: report.num_drops,
            last_frame_delay_ms: report.last_frame_delay,
        }
    }
}

/// Why a stream's bitrate moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateReason {
    /// A report counted late frames, or a late last frame: halved.
    Late,
    /// A report's last frame had under [`LOW_HEADROOM`] to spare: cut by
    /// a quarter.
    LowHeadroom,
    /// Two server drops within [`SERVER_DROP_WINDOW`]: cut by a quarter.
    ServerDrops,
    /// Reports stayed clean for [`CLEAN_PERIOD`]: raised by
    /// [`RAISE_PERCENT`].
    Clean,
}

/// A change of a stream's bitrate, in bits a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateChange {
    pub from: u32,
    pub to: u32,
    pub reason: RateReason,
}

/// A stream's target bitrate, and what deciding the next change needs to
/// remember. It knows nothing of the encoder: [`RateControl::adjust`]
/// says what the target becomes, and the stream applies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateControl {
    target: u32,
    cap: u32,
    /// When the current run of clean reports began, if one has, since
    /// the last change.
    clean_since: Option<Instant>,
    /// A server drop not yet followed by another within
    /// [`SERVER_DROP_WINDOW`].
    last_server_drop: Option<Instant>,
}

impl RateControl {
    /// Aim for `cap`, the most the stream may ever aim for, at first.
    pub fn new(cap: u32) -> Self {
        Self {
            target: cap,
            cap,
            clean_since: None,
            last_server_drop: None,
        }
    }

    /// The bitrate aimed for, in bits a second.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// The most the stream may aim for, in bits a second.
    pub fn cap(&self) -> u32 {
        self.cap
    }

    /// Account for `event` at `now`, and return the change it makes to
    /// the target, if any:
    ///
    /// - a report with late frames, or a late last frame, halves it;
    /// - a report whose last frame had under [`LOW_HEADROOM`] to spare
    ///   cuts it by a quarter;
    /// - a clean report that ends [`CLEAN_PERIOD`] of clean reports, with
    ///   no change since they began, raises it by [`RAISE_PERCENT`];
    /// - a server drop within [`SERVER_DROP_WINDOW`] of the one before
    ///   cuts it by a quarter.
    ///
    /// It never goes below [`MIN_STREAM_BITRATE_BPS`], nor above the cap.
    /// Any cut, or any server drop, starts the clean run again. A report
    /// of no frames says nothing, and changes nothing.
    pub fn adjust(&mut self, event: RateEvent, now: Instant) -> Option<RateChange> {
        let (target, reason) = match event {
            RateEvent::Report { frames: 0, .. } => return None,
            RateEvent::Report {
                drops,
                last_frame_delay_ms,
                ..
            } if drops > 0 || last_frame_delay_ms < 0 => (self.target / 2, RateReason::Late),
            RateEvent::Report {
                last_frame_delay_ms,
                ..
            } if i64::from(last_frame_delay_ms) < LOW_HEADROOM.as_millis() as i64 => {
                (quarter_off(self.target), RateReason::LowHeadroom)
            }
            RateEvent::Report { .. } => {
                let since = *self.clean_since.get_or_insert(now);
                if now.saturating_duration_since(since) < CLEAN_PERIOD {
                    return None;
                }
                let raised = u64::from(self.target) * u64::from(100 + RAISE_PERCENT) / 100;
                (u32::try_from(raised).unwrap_or(u32::MAX), RateReason::Clean)
            }
            RateEvent::ServerDrop => {
                self.clean_since = None;
                match self.last_server_drop.take() {
                    Some(last) if now.saturating_duration_since(last) <= SERVER_DROP_WINDOW => {
                        (quarter_off(self.target), RateReason::ServerDrops)
                    }
                    _ => {
                        self.last_server_drop = Some(now);
                        return None;
                    }
                }
            }
        };
        // A raise starts the next clean run now; a cut ends the run.
        self.clean_since = (reason == RateReason::Clean).then_some(now);
        let target = target.clamp(MIN_STREAM_BITRATE_BPS.min(self.cap), self.cap);
        if target == self.target {
            return None;
        }
        let change = RateChange {
            from: self.target,
            to: target,
            reason,
        };
        self.target = target;
        Some(change)
    }
}

/// Three quarters of `bps`.
fn quarter_off(bps: u32) -> u32 {
    (u64::from(bps) * 3 / 4) as u32
}

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
    /// [`Streams::may_start`]. The rectangle is shrunk to whole, even-sided
    /// tiles, as [`whole_tiles`] says. Returns the new stream, or `None` if
    /// none may start, nothing of `rect` is left, or what is left is too
    /// large to encode.
    /// Fails if the encoder cannot be built; the caller then turns
    /// streams off with [`Streams::disable`].
    pub fn start(&mut self, rect: Rectangle, size: Size, now: Instant) -> Result<Option<&Stream>> {
        if !self.may_start() {
            return Ok(None);
        }
        let Some(rect) = whole_tiles(rect, size) else {
            return Ok(None);
        };
        let (width, height) = (rect.width, rect.height);
        // Too large an area is no stream, not a failure of the encoder:
        // streams stay on for areas that fit.
        if !fits_encoder(width, height) {
            return Ok(None);
        }
        // Video, not screen content: openh264 holds a screen's bitrate by
        // encoding at its lowest QP and skipping frames, which plays a
        // 25 fps video at about 7 fps. A stream's area is photographic by
        // the heat map's choice. Skipping stays on, for real overload.
        let config = H264Config {
            content: H264Content::Video,
            ..H264Config::new(width, height, self.frame_rate)
        };
        let cap = target_bitrate_bps(width, height, self.frame_rate);
        let encoder = H264Encoder::new(config)
            .with_context(|| format!("creating an H.264 encoder for {rect}"))?;
        let id = self.next_id;
        self.next_id = (id + 1) % STREAM_IDS;
        self.created_any = true;
        Ok(Some(&*self.live.insert(Stream {
            id,
            rect,
            encoder: Box::new(encoder),
            rate: RateControl::new(cap),
            max_frame_rate: self.frame_rate as f32,
            started: now,
            frame_times: VecDeque::new(),
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

    /// Return a stream [`Streams::take`] took out. Fails if another
    /// stream has gone live meanwhile, which would be a bug: the caller
    /// closes the channel rather than lose track of either.
    pub fn put_back(&mut self, stream: Stream) -> Result<()> {
        ensure!(
            self.live.is_none(),
            "stream {} was put back over live stream {}",
            stream.id,
            self.live.as_ref().map_or(0, |live| live.id)
        );
        self.live = Some(stream);
        Ok(())
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
        let ours = stream.unique_id == Some(report.unique_id);
        if report.num_frames == 0 && report.num_drops == u32::MAX && (ours || report.unique_id == 0)
        {
            return ReportVerdict::CannotDecode;
        }
        if !ours {
            return ReportVerdict::OtherUniqueId;
        }
        stream.last_report = Some(report);
        ReportVerdict::Recorded
    }
}

/// What [`Stream::encode`] produced.
#[derive(Debug)]
pub struct Encoded {
    /// The frame, or `None` if the encoder skipped it.
    pub frame: Option<H264Frame>,
    /// The encoder's new frame rate, if [`Stream::pace`] changed it.
    pub frame_rate: Option<f32>,
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
    /// The bitrate it aims for.
    rate: RateControl,
    /// The frame rate the encoder was built for: the most
    /// [`Stream::pace`] tells it.
    max_frame_rate: f32,
    started: Instant,
    /// When each frame within [`FRAME_RATE_WINDOW`] was encoded.
    frame_times: VecDeque<Instant>,
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
            .field("rate", &self.rate)
            .field("frame_rate", &self.encoder.frame_rate())
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
        self.rate.target()
    }

    /// The most the stream may aim for, in bits a second.
    pub fn cap_bps(&self) -> u32 {
        self.rate.cap()
    }

    /// The frame rate the encoder budgets each frame by.
    pub fn frame_rate(&self) -> f32 {
        self.encoder.frame_rate()
    }

    /// Account for `event` at `now` with [`RateControl::adjust`], and
    /// tell the encoder of any change, which is returned.
    pub fn rate_event(&mut self, event: RateEvent, now: Instant) -> Result<Option<RateChange>> {
        let change = self.rate.adjust(event, now);
        if let Some(change) = change {
            self.encoder
                .set_bitrate(change.to)
                .context("changing the stream's bitrate")?;
        }
        Ok(change)
    }

    /// Record that a frame is about to be encoded at `now`, and keep the
    /// encoder's frame rate near the rate frames have come at over the
    /// last [`FRAME_RATE_WINDOW`]. The rate is measured once the stream is
    /// that old, kept between 1 and the configured rate, and passed on
    /// only when it has moved more than [`FRAME_RATE_HYSTERESIS_PERCENT`]
    /// from the encoder's. Returns the new rate when it changes.
    pub fn pace(&mut self, now: Instant) -> Result<Option<f32>> {
        while self
            .frame_times
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) >= FRAME_RATE_WINDOW)
        {
            self.frame_times.pop_front();
        }
        self.frame_times.push_back(now);
        if now.saturating_duration_since(self.started) < FRAME_RATE_WINDOW {
            return Ok(None);
        }
        let measured = (self.frame_times.len() as f32 / FRAME_RATE_WINDOW.as_secs_f32())
            .clamp(1.0, self.max_frame_rate.max(1.0));
        let current = self.encoder.frame_rate();
        if (measured - current).abs() * 100.0 <= current * FRAME_RATE_HYSTERESIS_PERCENT {
            return Ok(None);
        }
        self.encoder
            .set_frame_rate(measured)
            .context("changing the stream's frame rate")?;
        Ok(Some(measured))
    }

    /// The `unique_id` its reports carry, if they were asked for.
    pub fn unique_id(&self) -> Option<u32> {
        self.unique_id
    }

    /// The client's last report for this stream.
    pub fn last_report(&self) -> Option<&StreamReport> {
        self.last_report.as_ref()
    }

    /// The stream's tiles, of a surface of `size`: whole tiles, each
    /// clipped to the surface, and the only tiles it touches. Damage to
    /// them feeds the stream, its frames leave them lossy, and its end
    /// repairs them; see the module's notes on its tiles.
    pub fn tiles(&self, size: Size) -> Region {
        let mut tiles = Region::empty(size);
        tiles.add(self.rect);
        tiles
    }

    /// Record that a frame sent at `now` changed the stream.
    pub fn changed(&mut self, now: Instant) {
        self.last_change = now;
    }

    /// When the stream ends if nothing changes it first.
    pub fn quiet_at(&self) -> Instant {
        self.last_change + STREAM_QUIET
    }

    /// Count a frame that was due at `now` while the client's ack window
    /// was full, and so was never captured, and account for it with
    /// [`Stream::rate_event`]. Damage was pending on the stream, so it
    /// counts as a change too: a stream the client cannot keep up with
    /// is not a still one.
    pub fn server_drop(&mut self, now: Instant) -> Result<Option<RateChange>> {
        self.server_drops = self.server_drops.saturating_add(1);
        self.last_change = now;
        self.rate_event(RateEvent::ServerDrop, now)
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

    /// Encode `pixels`, a capture of the stream's rectangle, at `now`,
    /// first keeping the encoder's frame rate near the stream's with
    /// [`Stream::pace`]. The frame is `None` when the encoder skipped it
    /// to hold its bitrate, and nothing is to be sent. CPU-bound, so the
    /// display channel runs it on the blocking pool. Fails unless `pixels`
    /// is exactly the rectangle's size, or if the encoder fails.
    pub fn encode(&mut self, pixels: &Frame, now: Instant) -> Result<Encoded> {
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
        let frame_rate = self.pace(now)?;
        Ok(Encoded {
            frame: self.encoder.encode(&image)?,
            frame_rate,
        })
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
        let rect = Rectangle::new(64, 64, 128, 128);
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
                stream_height: 128,
                src_width: 128,
                src_height: 128,
                dest: Rect {
                    top: 64,
                    left: 64,
                    bottom: 192,
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
    fn the_rectangle_is_shrunk_to_whole_tiles_or_starts_nothing() {
        let mut streams = streams(BOTH);
        let stream = streams
            .start(Rectangle::new(40, 30, 300, 99), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        // Clipped to 216 x 99, then shrunk to the tiles inside it.
        assert_eq!(stream.rect(), Rectangle::new(64, 64, 192, 64));
        streams.end();

        // Outside the surface, a pixel wide, or less than a tile: nothing
        // starts.
        for rect in [
            Rectangle::new(300, 0, 64, 64),
            Rectangle::new(0, 0, 1, 64),
            Rectangle::new(201, 99, 300, 99),
        ] {
            assert!(
                streams.start(rect, SIZE, Instant::now()).unwrap().is_none(),
                "{rect}"
            );
        }
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
    fn cannot_decode_counts_zeroed_or_with_the_live_unique_id() {
        let mut streams = streams(BOTH);
        let unique_id = streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap()
            .unique_id()
            .unwrap();
        assert_eq!(streams.report(cannot_decode(1)), ReportVerdict::OtherStream);
        // spice-gtk zeroes the unique_id.
        assert_eq!(
            streams.report(cannot_decode(0)),
            ReportVerdict::CannotDecode
        );
        let with = |unique_id| StreamReport {
            unique_id,
            ..cannot_decode(0)
        };
        assert_eq!(streams.report(with(unique_id)), ReportVerdict::CannotDecode);
        // Another unique_id is an earlier stream's with the same id.
        let stale = if unique_id == u32::MAX {
            1
        } else {
            unique_id + 1
        };
        assert_eq!(streams.report(with(stale)), ReportVerdict::OtherUniqueId);
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
        let rect = Rectangle::new(64, 64, 128, 64);
        streams.start(rect, SIZE, Instant::now()).unwrap().unwrap();
        let mut stream = streams.take().unwrap();
        let pixels = padded_frame(rect.size(), 0x40);
        let frame = stream
            .encode(&pixels, Instant::now())
            .unwrap()
            .frame
            .expect("the first frame");
        assert!(frame.keyframe);

        let mut decoder = shakenfist_spice_compression::for_stream(
            CODEC_TYPE_H264,
            Arc::new(JpegDecoderRsDecoder::new()),
        )
        .unwrap();
        let decoded = decoder.decode(&frame.data).unwrap().unwrap();
        assert_eq!((decoded.width, decoded.height), (128, 64));
        streams.put_back(stream).unwrap();
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
            .encode(
                &padded_frame(
                    Size {
                        width: 66,
                        height: 64,
                    },
                    0,
                ),
                Instant::now(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("is not the"), "{err}");
    }

    #[test]
    fn a_stream_is_whole_even_tiles() {
        // A surface 643 x 387: its last tile column is 3 pixels wide and
        // its last row 3 tall.
        let size = Size {
            width: 643,
            height: 387,
        };
        let at = |x, y, w, h| whole_tiles(Rectangle::new(x, y, w, h), size);
        // Tile-aligned and even: as it is.
        assert_eq!(at(64, 64, 128, 192), Some(Rectangle::new(64, 64, 128, 192)));
        // Shrunk to the tiles it covers wholly.
        assert_eq!(
            at(60, 70, 140, 200),
            Some(Rectangle::new(64, 128, 128, 128))
        );
        // To the odd edges: 259 x 195 would be odd, so the 3-pixel last
        // column and row are left out.
        assert_eq!(
            at(384, 192, 259, 195),
            Some(Rectangle::new(384, 192, 256, 192))
        );
        // Beyond them, clipped first.
        assert_eq!(
            at(384, 192, 1000, 1000),
            Some(Rectangle::new(384, 192, 256, 192))
        );
        // An even edge tile stays: on a 642-wide surface, 2 pixels.
        let even = Size {
            width: 642,
            height: 386,
        };
        assert_eq!(
            whole_tiles(Rectangle::new(512, 256, 200, 200), even),
            Some(Rectangle::new(512, 256, 130, 130))
        );
        // No whole tile, or nothing left: none.
        assert_eq!(at(1, 1, 100, 100), None);
        assert_eq!(at(640, 0, 3, 64), None);
        assert_eq!(at(700, 0, 64, 64), None);

        // The stream's tiles are then exactly the tiles it touches, each
        // wholly inside it.
        let mut streams = streams(BOTH);
        let stream = streams
            .start(Rectangle::new(384, 192, 259, 195), size, Instant::now())
            .unwrap()
            .unwrap();
        assert_eq!(stream.rect(), Rectangle::new(384, 192, 256, 192));
        let tiles = stream.tiles(size);
        assert_eq!(tiles.tile_count(), 4 * 3);
        for (col, row) in tiles.tiles() {
            assert!(stream.rect().contains(&tiles.tile_rect(col, row)));
        }
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

        // A server drop counts as a change.
        assert_eq!(stream.server_drops(), 0);
        let drop = later + Duration::from_millis(100);
        stream.server_drop(drop).unwrap();
        stream.server_drop(drop).unwrap();
        assert_eq!(stream.server_drops(), 2);
        assert_eq!(stream.quiet_at(), drop + STREAM_QUIET);
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

    /// A cap above the floor, and well clear of it.
    const CAP: u32 = 4_000_000;

    fn rated(frames: u32, drops: u32, last_frame_delay_ms: i32) -> RateEvent {
        RateEvent::Report {
            frames,
            drops,
            last_frame_delay_ms,
        }
    }

    /// A report of five frames, none late, with plenty of headroom.
    fn clean() -> RateEvent {
        rated(5, 0, 60)
    }

    fn change(from: u32, to: u32, reason: RateReason) -> Option<RateChange> {
        Some(RateChange { from, to, reason })
    }

    #[test]
    fn a_stream_starts_at_and_is_capped_by_the_default_bitrate() {
        let mut streams = streams(BOTH);
        let rect = Rectangle::new(0, 0, 1920, 1080);
        let size = Size {
            width: 1920,
            height: 1080,
        };
        let stream = streams.start(rect, size, Instant::now()).unwrap().unwrap();
        let expected = target_bitrate_bps(1920, 1080, 30);
        assert_eq!(expected, 6_220_800);
        assert_eq!(
            (stream.target_bps(), stream.cap_bps()),
            (expected, expected)
        );
        assert_eq!(stream.encoder.bitrate_bps(), expected);
        assert_eq!(stream.frame_rate(), 30.0);
    }

    #[test]
    fn late_frames_halve_the_bitrate_down_to_the_floor() {
        let now = Instant::now();
        let mut rate = RateControl::new(CAP);
        assert_eq!(rate.target(), CAP);
        assert_eq!(
            rate.adjust(rated(5, 1, 30), now),
            change(CAP, CAP / 2, RateReason::Late)
        );
        // A late last frame alone does it too.
        assert_eq!(
            rate.adjust(rated(5, 0, -5), now),
            change(CAP / 2, CAP / 4, RateReason::Late)
        );
        // 500 kbit/s, then 250, which the floor holds at 256.
        rate.adjust(rated(5, 5, -100), now);
        assert_eq!(rate.target(), 500_000);
        assert_eq!(
            rate.adjust(rated(5, 5, -100), now),
            change(500_000, MIN_STREAM_BITRATE_BPS, RateReason::Late)
        );
        assert_eq!(rate.adjust(rated(5, 5, -100), now), None);
        assert_eq!(rate.target(), MIN_STREAM_BITRATE_BPS);
    }

    #[test]
    fn little_headroom_cuts_the_bitrate_by_a_quarter() {
        let now = Instant::now();
        let mut rate = RateControl::new(CAP);
        assert_eq!(
            rate.adjust(rated(5, 0, 19), now),
            change(CAP, CAP * 3 / 4, RateReason::LowHeadroom)
        );
        // Exactly the threshold is enough headroom.
        assert_eq!(rate.adjust(rated(5, 0, 20), now), None);
        // And the floor holds here too.
        for _ in 0..20 {
            rate.adjust(rated(5, 0, 0), now);
        }
        assert_eq!(rate.target(), MIN_STREAM_BITRATE_BPS);
    }

    #[test]
    fn two_seconds_of_clean_reports_raise_it_a_tenth_up_to_the_cap() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut rate = RateControl::new(CAP);
        // At the cap, clean reports change nothing.
        for ms in (0..5000).step_by(250) {
            assert_eq!(rate.adjust(clean(), at(ms)), None);
        }

        rate.adjust(rated(5, 1, 30), at(5000));
        assert_eq!(rate.target(), 2_000_000);
        // The run starts with the first clean report, and needs 2 s.
        assert_eq!(rate.adjust(clean(), at(5100)), None);
        assert_eq!(rate.adjust(clean(), at(7099)), None);
        assert_eq!(
            rate.adjust(clean(), at(7100)),
            change(2_000_000, 2_200_000, RateReason::Clean)
        );
        // The next raise needs 2 s more.
        assert_eq!(rate.adjust(clean(), at(8000)), None);
        assert_eq!(rate.adjust(clean(), at(9099)), None);
        assert_eq!(
            rate.adjust(clean(), at(9100)),
            change(2_200_000, 2_420_000, RateReason::Clean)
        );
        // Up to the cap, and no further.
        let mut ms = 9100;
        while rate.target() < CAP {
            ms += 2000;
            rate.adjust(clean(), at(ms));
        }
        assert_eq!(rate.target(), CAP);
        assert_eq!(rate.adjust(clean(), at(ms + 2000)), None);
        assert_eq!(rate.target(), CAP);
    }

    #[test]
    fn a_cut_or_a_server_drop_starts_the_clean_run_again() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut rate = RateControl::new(CAP);
        rate.adjust(rated(5, 1, 30), at(0));
        rate.adjust(clean(), at(100));
        rate.adjust(rated(5, 0, 10), at(1000));
        assert_eq!(rate.target(), 1_500_000);
        // The run began again at 1.1 s, not at 0.1 s.
        assert_eq!(rate.adjust(clean(), at(1100)), None);
        assert_eq!(rate.adjust(clean(), at(2500)), None);
        rate.adjust(RateEvent::ServerDrop, at(2600));
        assert_eq!(rate.adjust(clean(), at(3200)), None);
        assert_eq!(rate.adjust(clean(), at(5199)), None);
        assert_eq!(
            rate.adjust(clean(), at(5200)),
            change(1_500_000, 1_650_000, RateReason::Clean)
        );
    }

    #[test]
    fn two_server_drops_within_a_second_cut_it_by_a_quarter() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut rate = RateControl::new(CAP);
        // One alone does nothing, nor does one a second and a half later.
        assert_eq!(rate.adjust(RateEvent::ServerDrop, at(0)), None);
        assert_eq!(rate.adjust(RateEvent::ServerDrop, at(1500)), None);
        // Its pair, within the second, does.
        assert_eq!(
            rate.adjust(RateEvent::ServerDrop, at(2400)),
            change(CAP, CAP * 3 / 4, RateReason::ServerDrops)
        );
        // A pair is used up: the next drop starts another.
        assert_eq!(rate.adjust(RateEvent::ServerDrop, at(2410)), None);
        assert_eq!(
            rate.adjust(RateEvent::ServerDrop, at(2420)),
            change(CAP * 3 / 4, CAP * 9 / 16, RateReason::ServerDrops)
        );
        for ms in 0..100 {
            rate.adjust(RateEvent::ServerDrop, at(3000 + ms));
        }
        assert_eq!(rate.target(), MIN_STREAM_BITRATE_BPS);
    }

    #[test]
    fn a_report_of_no_frames_changes_nothing() {
        let now = Instant::now();
        let mut rate = RateControl::new(CAP);
        assert_eq!(rate.adjust(rated(0, 0, -50), now), None);
        assert_eq!(rate.adjust(rated(0, 3, 0), now), None);
        assert_eq!(rate.target(), CAP);
    }

    #[test]
    fn a_change_reaches_the_encoder() {
        let now = Instant::now();
        let mut streams = streams(BOTH);
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, now)
            .unwrap()
            .unwrap();
        let mut stream = streams.take().unwrap();
        let cap = stream.cap_bps();
        let report = StreamReport {
            num_drops: 2,
            ..report(0, 0)
        };
        assert_eq!(
            stream.rate_event(RateEvent::from(&report), now).unwrap(),
            change(cap, cap / 2, RateReason::Late)
        );
        assert_eq!(stream.target_bps(), cap / 2);
        assert_eq!(stream.encoder.bitrate_bps(), cap / 2);
    }

    #[test]
    fn the_encoder_frame_rate_follows_the_rate_frames_come_at() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut streams = streams(BOTH);
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, start)
            .unwrap()
            .unwrap();
        let mut stream = streams.take().unwrap();
        assert_eq!(stream.frame_rate(), 30.0);

        // Ten frames a second: nothing changes until the stream is a
        // second old, then the encoder is told.
        let mut changes = Vec::new();
        for ms in (0..1500).step_by(100) {
            if let Some(rate) = stream.pace(at(ms)).unwrap() {
                changes.push((ms, rate));
            }
        }
        assert_eq!(changes, vec![(1000, 10.0)]);
        assert_eq!(stream.frame_rate(), 10.0);
        assert_eq!(stream.encoder.frame_rate(), 10.0);

        // Ten or eleven a second is within the hysteresis.
        for ms in (1500..3000).step_by(95) {
            assert_eq!(stream.pace(at(ms)).unwrap(), None);
        }
        assert_eq!(stream.frame_rate(), 10.0);

        // Twenty a second is not.
        let mut last = None;
        for ms in (3000..4500).step_by(50) {
            last = stream.pace(at(ms)).unwrap().or(last);
        }
        assert_eq!(last, Some(20.0));

        // One frame in a second is held at one a second.
        assert_eq!(stream.pace(at(10_000)).unwrap(), Some(1.0));

        // Faster than configured is held at the configured rate.
        let mut streams = self::streams(BOTH);
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, start)
            .unwrap()
            .unwrap();
        let mut stream = streams.take().unwrap();
        for ms in (0..1500).step_by(10) {
            assert_eq!(stream.pace(at(ms)).unwrap(), None);
        }
        assert_eq!(stream.frame_rate(), 30.0);

        // A 25 fps video in a stream configured for 30 is told 25: the
        // budget would otherwise be a sixth short.
        let mut streams = self::streams(BOTH);
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, start)
            .unwrap()
            .unwrap();
        let mut stream = streams.take().unwrap();
        let mut last = None;
        for ms in (0..1500).step_by(40) {
            last = stream.pace(at(ms)).unwrap().or(last);
        }
        assert_eq!(last, Some(25.0));
    }

    #[test]
    fn a_stream_is_encoded_as_video_and_may_skip() {
        let mut streams = streams(BOTH);
        let stream = streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, Instant::now())
            .unwrap()
            .unwrap();
        let config = stream.encoder.config();
        assert_eq!(config.content, H264Content::Video);
        assert!(config.skip_frames);
    }

    #[test]
    fn an_area_too_large_to_encode_is_no_stream_and_leaves_streams_on() {
        assert!(fits_encoder(3840, 2160));
        assert!(fits_encoder(2160, 3840));
        // Rounded down to even first.
        assert!(fits_encoder(3841, 2161));
        assert!(!fits_encoder(3842, 2160));
        assert!(!fits_encoder(3840, 2162));
        assert!(!fits_encoder(2162, 2162));

        let size = Size {
            width: 5120,
            height: 2880,
        };
        let mut streams = streams(BOTH);
        let now = Instant::now();
        assert!(
            streams
                .start(Rectangle::new(0, 0, 4096, 256), size, now)
                .unwrap()
                .is_none()
        );
        assert!(streams.enabled(), "too large an area turned streams off");
        // The largest the encoder takes: a whole 4K surface, whose bottom
        // edge is not on a tile boundary.
        let uhd = Size {
            width: 3840,
            height: 2160,
        };
        let stream = streams
            .start(Rectangle::full(uhd), uhd, now)
            .unwrap()
            .unwrap();
        assert_eq!(stream.rect(), Rectangle::full(uhd));
    }

    #[test]
    fn a_stream_put_back_over_a_live_one_is_an_error() {
        let now = Instant::now();
        let mut streams = streams(BOTH);
        streams
            .start(Rectangle::new(0, 0, 64, 64), SIZE, now)
            .unwrap()
            .unwrap();
        let first = streams.take().unwrap();
        streams
            .start(Rectangle::new(64, 0, 64, 64), SIZE, now)
            .unwrap()
            .unwrap();
        let err = streams.put_back(first).unwrap_err();
        assert!(
            err.to_string().contains("put back over live stream 1"),
            "{err}"
        );
        assert_eq!(streams.live().unwrap().id(), 1);
    }

    #[test]
    fn extreme_reports_are_late_frames() {
        let now = Instant::now();
        // The latest a frame can be.
        let latest = StreamReport {
            last_frame_delay: i32::MIN,
            ..report(0, 0)
        };
        let mut rate = RateControl::new(CAP);
        assert_eq!(
            rate.adjust(RateEvent::from(&latest), now),
            change(CAP, CAP / 2, RateReason::Late)
        );
        // More drops than frames is nonsense, but drops all the same.
        let nonsense = StreamReport {
            num_frames: 5,
            num_drops: 9,
            last_frame_delay: 60,
            ..report(0, 0)
        };
        assert_eq!(
            rate.adjust(RateEvent::from(&nonsense), now),
            change(CAP / 2, CAP / 4, RateReason::Late)
        );
        assert_eq!(
            RateEvent::from(&nonsense),
            rated(5, 9, 60),
            "the report's fields are taken as they are"
        );
    }
}
