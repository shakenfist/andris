//! The display channel.
//!
//! What it sends and when, as a user sees it, is described in
//! [`docs/running.md`][running], under "What a client gets". This says
//! what the code relies on.
//!
//! # Frames
//!
//! After `SET_ACK` ([`ACK_WINDOW`]) and the client's `INIT`, damage drives
//! everything. The channel subscribes to damage before `INIT`, and a new
//! subscription starts with the whole desktop pending, which the first
//! frame takes. A frame starts only when damage is pending, at least
//! `frame_interval` has passed since the last one started (a cap on the
//! rate, not a polling period), and the ack window has room. Damage that
//! arrives meanwhile merges in the subscription; pixels are never queued,
//! so a frame always captures them fresh. A frame whose damage changed
//! nothing sends nothing, not even `MARK`.
//!
//! A frame for damage of another size than the surface, or whose capture
//! is of another size, captures the whole desktop and re-runs the start
//! sequence ([`Display::start_surface`]) at the capture's own size, so the
//! surface is always the size of the frame drawn on it.
//!
//! # Cancel-safety
//!
//! The main loop's `select!` drops every branch it does not take, so each
//! must lose nothing when dropped: [`ChannelConn::recv`] and
//! [`ChannelConn::wait_for_room`] keep what they read,
//! [`DamageSubscription::changed`] loses nothing, the sleeps are made
//! afresh each time round, and an interval's `tick()` is cancel-safe.
//! Frames, refreshes, the scrub and the end of a quiet stream run in
//! branch bodies, after the `select!` has chosen, so nothing drops them
//! part way but the end of the channel.
//!
//! # The shadow
//!
//! The [`UpdateEncoder`]'s shadow holds the true pixels of everything the
//! client was sent, even where it was sent lossily. So any part of the
//! surface can be sent again without capturing it, and the scrub can
//! compare a capture with what the client should show. A new surface
//! starts with a new shadow and no lossy tiles.
//!
//! # Failed captures
//!
//! A failed capture skips its frame and puts the damage it took back.
//! After any failed capture, a frame's or the scrub's, nothing is captured
//! for the longer of `frame_interval` and [`CAPTURE_RETRY`]. The gap makes
//! the failures a channel tolerates, [`MAX_CAPTURE_FAILURES`] in a row,
//! span a fixed time rather than a number of frame intervals, long enough
//! to ride out an RDP resize of an xrdp session. The channel closes on the
//! last of them, or at once if the platform says the desktop is gone.
//!
//! # Lossy tiles and the refresh
//!
//! Every draw sent is recorded in [`LossyTiles`]: a lossy one makes the
//! tiles it touches due for a refresh `lossless_delay` later, and a
//! lossless one makes the tiles it covers exact. Damage a frame takes
//! postpones the lossy tiles it touches. A refresh sends the tiles that
//! are due losslessly from the shadow, as a frame of its own that waits
//! for the rate cap and the ack window, behind any pending damage; it
//! then marks them exact. So a still screen always ends up exact.
//!
//! # Video streams
//!
//! A client with both the `MULTI_CODEC` and `CODEC_H264` capabilities may
//! be sent one H.264 stream at a time, which [`Streams`] holds; the
//! stream module says what its messages carry.
//!
//! - **Starting.** Each frame that changed something is recorded in the
//!   [`HeatMap`]: the tiles it changed, and which of them went out
//!   photographic. When no stream lives, the largest area the heat map
//!   finds starts one.
//! - **Feeding.** While a stream lives, damage to the tiles it covers
//!   wholly is taken out of the frame's draws. If any were damaged, the
//!   stream's rectangle is captured, brought into the shadow, and sent as
//!   one `STREAM_DATA`, encoded on the blocking pool and stamped from the
//!   session's clock, the one `MAIN_INIT` gave the client. Its tiles are
//!   then lossy, due only once the stream has ended. A frame due while
//!   the ack window is full is not captured, as for draws, and counts as
//!   a server drop.
//! - **Ending.** A stream ends when nothing has changed it for
//!   [`STREAM_QUIET`], or when an area that touches it has grown past it.
//!   Its tiles then fall due at once, so the lossless refresh repairs the
//!   area, and their heat is forgotten, so that it streams again only
//!   after a fresh run of frames. A report that the client cannot decode
//!   the stream, or a failure of the encoder, ends it too, and turns
//!   streams off for the rest of the channel, with one warning.
//!   Replacing the surface ends every stream first, with
//!   `STREAM_DESTROY_ALL`, once any stream has been created.
//!
//! [running]: https://github.com/shakenfist/andris/blob/develop/docs/running.md
//! [`LossyTiles`]: super::updates::LossyTiles
//! [`HeatMap`]: super::updates::HeatMap
//! [`STREAM_QUIET`]: super::updates::STREAM_QUIET

use std::time::Duration;

use anyhow::{Context, Result, bail};
use shakenfist_spice_protocol::constants::{image_compression, surface_fmt};
use shakenfist_spice_protocol::messages::{
    DisplayHead, DisplayInit, DisplayMonitorsConfig, PreferredCompression, PreferredVideoCodecType,
    ReceivedMessage, StreamReport, SurfaceCreate, SurfaceDestroy, WireType,
};
use shakenfist_spice_protocol::{capabilities, display_client};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use super::channel::{ACK_WINDOW, ChannelConn, ChannelContext, ClientCaps};
use super::opcodes::DisplayOpcode;
use super::updates::{
    Choice, Draw, EncodeOptions, Encoded, HeatMap, JpegQuality, LossyTiles, RateChange, RateEvent,
    ReportVerdict, STREAM_QUIET, SURFACE_ID, Stream, StreamCaps, Streams, Update, UpdateEncoder,
    choose,
};
use crate::config::DEFAULT_FRAME_INTERVAL_MS;
use crate::platform::{self, Capture, DamageSubscription, Rectangle, Region, Size, TILE};

/// `SPICE_SURFACE_FLAGS_PRIMARY` (spice-protocol enums.h), which ryll's
/// protocol crate does not name: the surface is the display itself.
pub const SURFACE_FLAGS_PRIMARY: u32 = 1 << 0;

/// How many captures in a row may fail before the channel gives up.
pub const MAX_CAPTURE_FAILURES: u32 = 10;

/// The shortest wait after a failed capture before the next, unless
/// `frame_interval` is longer. With [`MAX_CAPTURE_FAILURES`], it lets a
/// channel ride out about two seconds of failures, as an RDP resize of an
/// xrdp session can cause.
pub const CAPTURE_RETRY: Duration = Duration::from_millis(200);

/// The most rectangles a frame captures its damage as. More than a frame
/// sends ([`MAX_RECTS_PER_FRAME`]), since a capture of a few more, smaller
/// rectangles reads less of the desktop than one of fewer, larger ones.
///
/// [`MAX_RECTS_PER_FRAME`]: super::updates::MAX_RECTS_PER_FRAME
pub const MAX_CAPTURE_RECTS: usize = 64;

/// How long the scrub takes to re-read the whole desktop, unless the
/// server's options say otherwise.
pub const SCRUB_PERIOD: Duration = Duration::from_secs(4);

/// How many ticks the scrub spreads one pass over the desktop across.
pub const SCRUB_SLICES: u32 = 16;

/// How long an area sent as JPEG must go without damage before it is
/// sent again losslessly, unless the server's options say otherwise.
pub const LOSSLESS_DELAY: Duration = Duration::from_millis(500);

/// How a display channel paces its frames, its scrub and its lossless
/// refreshes, and whether it may send JPEG. The server hands every
/// display channel the same options. [`DisplayOptions::default`] gives
/// what andris runs with when the command line does not say otherwise;
/// tests shorten the times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayOptions {
    /// The shortest time between the starts of two frames: a cap on the
    /// frame rate, not a polling period.
    pub frame_interval: Duration,
    /// How long the scrub takes to re-read the whole desktop.
    pub scrub_period: Duration,
    /// How long an area sent as JPEG must stay still before it is sent
    /// again losslessly.
    pub lossless_delay: Duration,
    /// The quality to send JPEG at, or `None` if JPEG must not be sent
    /// (`--jpeg-quality 0`).
    pub jpeg_quality: Option<JpegQuality>,
}

impl Default for DisplayOptions {
    /// `--frame-interval-ms` and `--jpeg-quality` at their defaults,
    /// [`SCRUB_PERIOD`] and [`LOSSLESS_DELAY`].
    fn default() -> Self {
        Self {
            frame_interval: Duration::from_millis(DEFAULT_FRAME_INTERVAL_MS),
            scrub_period: SCRUB_PERIOD,
            lossless_delay: LOSSLESS_DELAY,
            jpeg_quality: Some(JpegQuality::DEFAULT),
        }
    }
}

/// Serve a display channel until the client closes it.
pub async fn run(ctx: ChannelContext, mut conn: ChannelConn<DisplayOpcode>) -> Result<()> {
    conn.start_acks(ACK_WINDOW).await?;
    // Subscribed before INIT, so that no damage reported from here on is
    // missed. The subscription starts with the whole desktop pending,
    // which the first frame takes.
    let damage = ctx
        .platform
        .screen
        .subscribe_damage()
        .context("subscribing to the desktop's damage")?;
    let scrub_every = scrub_tick(ctx.display.scrub_period);
    let lossy = LossyTiles::new(ctx.display.lossless_delay);
    let streams = Streams::new(stream_caps(&ctx.caps), ctx.display.frame_interval);
    let mut display = Display {
        ctx,
        conn,
        preferred_compression: None,
        initialised: false,
        surface: None,
        encoder: UpdateEncoder::new(),
        capture_failures: 0,
        damage,
        damage_pending: false,
        next_frame: Instant::now(),
        capture_retry: Instant::now(),
        scrub_top: 0,
        lossy,
        streams,
        heat: HeatMap::new(Size {
            width: 0,
            height: 0,
        }),
        next_drop: Instant::now(),
    };

    // Wait for INIT, handling whatever else arrives first.
    while !display.initialised {
        let Some(message) = display.conn.recv().await? else {
            return Ok(());
        };
        display.handle(&message).await?;
    }

    let mut scrub_ticks = tokio::time::interval_at(Instant::now() + scrub_every, scrub_every);
    scrub_ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let now = Instant::now();
        let cap_open = now >= display.next_frame;
        let room = display.conn.may_send();
        if display.damage_pending && cap_open && room {
            // Boxed, as are a message's handling and a server drop,
            // because they are the largest futures the loop awaits, and a
            // debug build keeps copies of each on the stack.
            Box::pin(display.frame()).await?;
            continue;
        }
        // A frame due now would feed the stream, but the ack window is
        // full: it is not captured, and damage merges meanwhile, as for
        // draws. It counts as a server drop, once a frame interval.
        let stream_blocked =
            display.damage_pending && cap_open && !room && display.stream_frame_due();
        if stream_blocked && now >= display.next_drop {
            Box::pin(display.server_drop(now)).await?;
        }
        // A refresh is a frame too, but pending damage goes first.
        let refresh_at = display.lossy.next_due();
        let refresh_due = refresh_at.is_some_and(|at| at <= now);
        if refresh_due && cap_open && room {
            display.refresh().await?;
            continue;
        }
        // Something blocks a frame or a refresh, or neither is wanted.
        // Wait for what would unblock it, for the client, for the next
        // lossy tile to fall due, and for the scrub, but never for damage
        // already known to be pending: changed() would return at once,
        // and the loop would spin.
        let pending = display.damage_pending;
        let wanted = pending || refresh_due;
        let await_room = wanted && cap_open;
        let next_frame = display.next_frame;
        let next_drop = display.next_drop;
        let quiet_at = display.streams.live().map(Stream::quiet_at);
        // Every branch is cancel-safe: next_incoming wraps recv() or
        // wait_for_room(), each of which keeps whatever it read; changed()
        // loses nothing when dropped; each sleep is made afresh each time
        // round; and an interval's tick() is cancel-safe.
        tokio::select! {
            incoming = next_incoming(&mut display.conn, await_room) => match incoming? {
                Incoming::Message(message) => Box::pin(display.handle(&message)).await?,
                Incoming::Closed => return Ok(()),
                Incoming::Room => {}
            },
            () = display.damage.changed(), if !pending => display.damage_pending = true,
            () = tokio::time::sleep_until(next_frame), if wanted && !cap_open => {}
            () = tokio::time::sleep_until(refresh_at.unwrap_or(now)),
                if refresh_at.is_some() && !refresh_due => {}
            _ = scrub_ticks.tick() => display.scrub().await?,
            () = tokio::time::sleep_until(next_drop), if stream_blocked && next_drop > now => {}
            () = tokio::time::sleep_until(quiet_at.unwrap_or(now)), if quiet_at.is_some() => {
                display.end_quiet_stream().await?;
            }
        }
    }
}

/// The scrub's tick: [`SCRUB_SLICES`] of them make `period`. Never zero,
/// which a tokio interval refuses.
fn scrub_tick(period: Duration) -> Duration {
    (period / SCRUB_SLICES).max(Duration::from_millis(1))
}

/// The height of one scrub slice of a desktop `height` rows tall: whole
/// [`TILE`] rows, enough of them that [`SCRUB_SLICES`] slices cover it.
fn scrub_slice_height(height: u32) -> u32 {
    height.div_ceil(TILE).div_ceil(SCRUB_SLICES).max(1) * TILE
}

/// What the client's display capabilities allow of streams: H.264, the
/// only codec andris encodes, needs both `MULTI_CODEC` and `CODEC_H264`,
/// and reports need `STREAM_REPORT`.
fn stream_caps(caps: &ClientCaps) -> StreamCaps {
    StreamCaps {
        h264: caps
            .has_channel_mask(capabilities::DISPLAY_MULTI_CODEC | capabilities::DISPLAY_CODEC_H264),
        reports: caps.has_channel_mask(capabilities::DISPLAY_STREAM_REPORT),
    }
}

/// What the client's side of the main loop's select produced.
enum Incoming {
    /// A message for the display channel.
    Message(ReceivedMessage),
    /// The client closed the channel.
    Closed,
    /// The ack window has room again.
    Room,
}

/// Read the client: until the ack window has room if `await_room`, and
/// otherwise until a message arrives. Cancel-safe, as
/// [`ChannelConn::recv`] and [`ChannelConn::wait_for_room`] are.
///
/// `ACK`s are consumed inside the connection and never returned by
/// [`ChannelConn::recv`], so a channel waiting for room must read the
/// client through [`ChannelConn::wait_for_room`], which returns once an
/// `ACK` makes room and keeps any other message for the next `recv`.
async fn next_incoming(
    conn: &mut ChannelConn<DisplayOpcode>,
    await_room: bool,
) -> Result<Incoming> {
    if await_room {
        conn.wait_for_room().await?;
        return Ok(Incoming::Room);
    }
    Ok(match conn.recv().await? {
        Some(message) => Incoming::Message(message),
        None => Incoming::Closed,
    })
}

/// One display channel's state.
struct Display {
    ctx: ChannelContext,
    conn: ChannelConn<DisplayOpcode>,
    /// The client's last `PREFERRED_COMPRESSION`, an `image_compression`
    /// value.
    preferred_compression: Option<u8>,
    /// Whether the client's `INIT` has arrived.
    initialised: bool,
    /// The primary surface's size, once it has been created.
    surface: Option<Size>,
    /// The shadow of what the client's surface holds.
    encoder: UpdateEncoder,
    /// How many captures in a row have failed.
    capture_failures: u32,
    /// The desktop's damage, as this channel sees it.
    damage: DamageSubscription,
    /// Whether [`DamageSubscription::changed`] has said damage is pending
    /// since the last frame took it.
    damage_pending: bool,
    /// The earliest the next frame may start.
    next_frame: Instant,
    /// The earliest the next capture of any kind may start, after a
    /// failed one.
    capture_retry: Instant,
    /// The top row of the scrub's next slice.
    scrub_top: u32,
    /// The tiles last sent lossily, and when each is due to be sent again
    /// losslessly.
    lossy: LossyTiles,
    /// The video stream, if one is live, and whether one may start.
    streams: Streams,
    /// The surface's recent changes, from which streams start.
    heat: HeatMap,
    /// The earliest the next server drop may be counted.
    next_drop: Instant,
}

impl Display {
    /// Handle one message from the client. `PREFERRED_COMPRESSION` takes
    /// effect from the next draw (see [`Display::encode_options`]);
    /// `STREAM_REPORT` is recorded against the live stream, or ends it if
    /// the client cannot decode it; `PREFERRED_VIDEO_CODEC_TYPE` is parsed
    /// and ignored. A malformed message fails, closing the channel; an
    /// unknown one is ignored.
    async fn handle(&mut self, message: &ReceivedMessage) -> Result<()> {
        let session = self.conn.session().id();
        match message.header.message_type {
            display_client::INIT => {
                let init = DisplayInit::decode(message.payload()).context("a malformed INIT")?;
                if init.cache_size < 0 {
                    bail!("INIT claims a pixmap cache of {} pixels", init.cache_size);
                }
                if init.glz_dict_window < 0 {
                    bail!(
                        "INIT claims a GLZ window of {} pixels",
                        init.glz_dict_window
                    );
                }
                if self.initialised {
                    debug!(session, "a second display INIT; ignored");
                }
                self.initialised = true;
            }
            display_client::PREFERRED_COMPRESSION => {
                let preferred = PreferredCompression::decode(message.payload())
                    .context("a malformed PREFERRED_COMPRESSION")?;
                debug!(
                    session,
                    compression = preferred.image_compression,
                    "the client's preferred compression"
                );
                self.preferred_compression = Some(preferred.image_compression);
            }
            display_client::PREFERRED_VIDEO_CODEC_TYPE => {
                // Andris encodes only H.264, so the client's order among
                // the codecs it can decode changes nothing: its caps
                // decide (see stream_caps).
                PreferredVideoCodecType::decode(message.payload())
                    .context("a malformed PREFERRED_VIDEO_CODEC_TYPE")?;
                self.conn.ignore(message);
            }
            display_client::STREAM_REPORT => {
                let report =
                    StreamReport::decode(message.payload()).context("a malformed STREAM_REPORT")?;
                self.stream_report(report).await?;
            }
            _ => self.conn.ignore(message),
        }
        Ok(())
    }

    /// How images may be encoded: LZ4 if the client can decode it and has
    /// not asked for no compression, and JPEG only if the client leaves
    /// the choice to the server and `--jpeg-quality` allows it.
    fn encode_options(&self) -> EncodeOptions {
        let preferred = self.preferred_compression;
        let jpeg = match preferred {
            None | Some(image_compression::AUTO_GLZ | image_compression::AUTO_LZ) => {
                self.ctx.display.jpeg_quality
            }
            Some(_) => None,
        };
        EncodeOptions {
            lz4: self
                .ctx
                .caps
                .has_channel_mask(capabilities::DISPLAY_LZ4_COMPRESSION)
                && preferred != Some(image_compression::OFF),
            jpeg,
        }
    }

    /// Act on a client's `STREAM_REPORT`.
    async fn stream_report(&mut self, report: StreamReport) -> Result<()> {
        let session = self.conn.session().id();
        let (stream, unique_id) = (report.stream_id, report.unique_id);
        debug!(
            session,
            stream,
            unique_id,
            frames = report.num_frames,
            drops = report.num_drops,
            last_frame_delay_ms = report.last_frame_delay,
            "a stream report"
        );
        let event = RateEvent::from(&report);
        match self.streams.report(report) {
            ReportVerdict::Recorded => {
                if let Some(stream) = self.streams.live_mut() {
                    let changed = stream.rate_event(event, Instant::now());
                    self.rate_changed(changed).await?;
                }
            }
            ReportVerdict::CannotDecode => {
                let live = self.streams.end();
                self.disable_streams(live, "the client cannot decode it")
                    .await?;
            }
            ReportVerdict::NoStream => {
                debug!(session, stream, "a report with no stream live; ignored");
            }
            ReportVerdict::OtherStream => {
                debug!(session, stream, "a report for another stream; ignored");
            }
            ReportVerdict::OtherUniqueId => {
                debug!(
                    session,
                    stream, unique_id, "a report from an earlier stream with this id; ignored"
                );
            }
        }
        Ok(())
    }

    /// Wait for room in the ack window, then send one message.
    async fn send(&mut self, op: DisplayOpcode, body: &[u8]) -> Result<()> {
        self.conn.wait_for_room().await?;
        self.conn.send(op, body).await
    }

    /// [`Display::send`] for a typed body.
    async fn send_msg<T: WireType>(&mut self, op: DisplayOpcode, message: &T) -> Result<()> {
        self.conn.wait_for_room().await?;
        self.conn.send_msg(op, message).await
    }

    /// Capture `rects`, or the whole desktop if `None`. Captures may
    /// block, so this runs on the blocking pool. It borrows `self` only to
    /// start, so the future stays `Send` although the connection is not
    /// `Sync`.
    fn capture(
        &self,
        rects: Option<Vec<Rectangle>>,
    ) -> impl Future<Output = Result<Capture>> + Send + 'static {
        let screen = self.ctx.platform.screen.clone();
        async move {
            tokio::task::spawn_blocking(move || match rects {
                Some(rects) => screen.capture(&rects),
                None => screen.capture_all(),
            })
            .await
            .context("the capture task failed")?
            .context("capturing the desktop")
        }
    }

    /// [`Display::capture`], counting failures. `None` skips this
    /// capture. Fails if the desktop is gone, or once
    /// [`MAX_CAPTURE_FAILURES`] captures in a row have failed.
    async fn capture_or_skip(&mut self, rects: Option<Vec<Rectangle>>) -> Result<Option<Capture>> {
        let session = self.conn.session().id();
        let started = Instant::now();
        match self.capture(rects).await {
            Ok(capture) => {
                self.conn
                    .session()
                    .counters()
                    .capture_done(started.elapsed());
                if self.capture_failures > 0 {
                    info!(
                        session,
                        failures = self.capture_failures,
                        "capturing the desktop works again"
                    );
                    self.capture_failures = 0;
                }
                Ok(Some(capture))
            }
            Err(e) if platform::is_gone(&e) => Err(e),
            Err(e) => {
                self.capture_failures += 1;
                // Hold off every capture, the frames' and the scrub's.
                let retry = Instant::now() + self.ctx.display.frame_interval.max(CAPTURE_RETRY);
                self.capture_retry = retry;
                self.next_frame = self.next_frame.max(retry);
                if self.capture_failures >= MAX_CAPTURE_FAILURES {
                    return Err(
                        e.context(format!("{MAX_CAPTURE_FAILURES} captures in a row failed"))
                    );
                }
                if self.capture_failures == 1 {
                    warn!(
                        session,
                        error = format!("{e:#}"),
                        "capturing the desktop failed; skipping frames until it works"
                    );
                }
                Ok(None)
            }
        }
    }

    /// Take the pending damage, capture it, and send what changed and
    /// then `MARK`; or, for the first frame or after a resize, run the
    /// start sequence from a capture of the whole desktop. A failed
    /// capture sends nothing, and puts the damage back to be taken again.
    ///
    /// While a stream lives, damage to the tiles it covers is sent as a
    /// stream frame instead, after the draws. What the frame changed then
    /// goes into the heat map, which may start or end a stream.
    async fn frame(&mut self) -> Result<()> {
        self.damage_pending = false;
        let taken = self.damage.take().context("taking the desktop's damage")?;
        if taken.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        self.next_frame = now + self.ctx.display.frame_interval;

        // Without a surface of the region's size, the surface is created
        // afresh, which needs all of the desktop, and ends any stream.
        let whole = self.surface != Some(taken.size());
        let mut region = taken.clone();
        let mut streamed = false;
        if !whole && let Some(stream) = self.streams.live() {
            for (col, row) in stream.covered(taken.size()).tiles() {
                streamed |= region.contains_tile(col, row);
                region.remove_tile(col, row);
            }
        }
        // Damage postpones the refresh of the lossy tiles it touches,
        // whatever the capture finds. A stream's tiles are due only once
        // it ends.
        self.lossy.touch(&region, now);

        let mut changed = Region::empty(taken.size());
        let mut photographic = Region::empty(taken.size());
        if !region.is_empty() {
            let rects = (!whole).then(|| region.rectangles(MAX_CAPTURE_RECTS));
            let Some(mut capture) = self.capture_or_skip(rects).await? else {
                return self.damage.restore(&taken);
            };
            if !whole && self.surface != Some(capture.size()) {
                // The desktop resized before its damage said so.
                let Some(all) = self.capture_or_skip(None).await? else {
                    return self.damage.restore(&taken);
                };
                capture = all;
            }

            if self.surface != Some(capture.size()) {
                return self.replace_surface(capture).await;
            }
            let update = self.send_update(capture).await?;
            if !update.draws.is_empty() {
                self.send(DisplayOpcode::Mark, &[]).await?;
                self.conn.session().counters().frame_sent(false);
            }
            for draw in update.draws.iter().filter(|draw| draw.is_lossy()) {
                photographic.add(draw.rect);
            }
            changed = update.changed;
        }
        if streamed && let Some(frame) = self.stream_frame(&taken, now).await? {
            // A stream's frame is photographic.
            changed.union(&frame);
            photographic.union(&frame);
        }
        self.detect(now, &changed, &photographic).await
    }

    /// Destroy the surface, if there is one, and start one at `capture`'s
    /// size. `capture` must be of the whole desktop.
    async fn replace_surface(&mut self, capture: Capture) -> Result<()> {
        if let Some(old) = self.surface.take() {
            debug!(
                session = self.conn.session().id(),
                from = %old,
                to = %capture.size(),
                "the desktop changed size; replacing the surface"
            );
            // Streams are drawn on the surface, so they end first. A
            // client never sent a stream is told nothing of them.
            if self.streams.live().is_some() {
                self.conn.session().counters().stream_destroyed();
            }
            if self.streams.end_all() {
                self.send(DisplayOpcode::StreamDestroyAll, &[]).await?;
            }
            self.send_msg(
                DisplayOpcode::SurfaceDestroy,
                &SurfaceDestroy {
                    surface_id: SURFACE_ID,
                },
            )
            .await?;
        }
        self.start_surface(capture).await
    }

    /// Create the primary surface at `capture`'s size and draw all of it,
    /// then describe the monitor and mark the display ready. `capture`
    /// must be of the whole desktop.
    ///
    /// `MONITORS_CONFIG`, with one head, goes only to a client with that
    /// capability. `INVAL_ALL_PALETTES` is never sent: andris sends no
    /// palettes, and keeps no pixmap cache or GLZ dictionary for `INIT` to
    /// size.
    async fn start_surface(&mut self, capture: Capture) -> Result<()> {
        let size = capture.size();
        self.send_msg(
            DisplayOpcode::SurfaceCreate,
            &SurfaceCreate {
                surface_id: SURFACE_ID,
                width: size.width,
                height: size.height,
                format: surface_fmt::FMT_32_XRGB,
                flags: SURFACE_FLAGS_PRIMARY,
            },
        )
        .await?;
        self.surface = Some(size);
        // A new surface starts empty, so the client has nothing the old
        // shadow describes, lossily or not.
        self.encoder.reset();
        self.lossy.reset(size);
        self.heat.reset(size);
        self.send_update(capture).await?;
        if self
            .ctx
            .caps
            .has_channel_mask(capabilities::DISPLAY_MONITORS_CONFIG)
        {
            let config = DisplayMonitorsConfig {
                max_allowed: 1,
                heads: vec![DisplayHead {
                    monitor_id: 0,
                    surface_id: SURFACE_ID,
                    width: size.width,
                    height: size.height,
                    x: 0,
                    y: 0,
                    flags: 0,
                }],
            };
            self.send_msg(DisplayOpcode::MonitorsConfig, &config)
                .await?;
        }
        self.send(DisplayOpcode::Mark, &[]).await?;
        self.conn.session().counters().frame_sent(false);
        Ok(())
    }

    /// Run `f` on the encoder on the blocking pool, since comparing and
    /// encoding are CPU-bound, and put the encoder back.
    ///
    /// The encoder moves into the task, leaving a default one behind
    /// until it returns. If the task panics, the encoder is lost with it:
    /// this fails, the failure closes the channel, and the default
    /// encoder left behind is never used. Dropping this future before
    /// the task ends loses the encoder too, which happens only when the
    /// channel itself is dropped.
    async fn with_encoder<R: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut UpdateEncoder) -> R + Send + 'static,
    ) -> Result<R> {
        let mut encoder = std::mem::take(&mut self.encoder);
        let (encoder, result) = tokio::task::spawn_blocking(move || {
            let result = f(&mut encoder);
            (encoder, result)
        })
        .await
        .context("the encoding task failed")?;
        self.encoder = encoder;
        Ok(result)
    }

    /// Send what in `capture` differs from what the client was last
    /// sent: all of it after [`UpdateEncoder::reset`]. Returns what was
    /// sent.
    async fn send_update(&mut self, capture: Capture) -> Result<Update> {
        let options = self.encode_options();
        let started = Instant::now();
        let update = self
            .with_encoder(move |encoder| encoder.update(&capture, options))
            .await?
            .context("encoding an update")?;
        let counters = self.conn.session().counters();
        counters.encode_done(started.elapsed());
        counters.area_updated(update.captured_area, update.changed_area);
        self.send_draws(&update.draws).await?;
        Ok(update)
    }

    /// Send `draws`, and record which tiles they leave lossy or exact.
    async fn send_draws(&mut self, draws: &[Draw]) -> Result<()> {
        for draw in draws {
            self.send(DisplayOpcode::DrawCopy, &draw.body).await?;
            self.conn
                .session()
                .counters()
                .draw_sent(draw.encoding, draw.encoded_bytes());
        }
        self.lossy.record(draws, Instant::now());
        Ok(())
    }

    /// Send the lossy tiles that are due again losslessly, from the
    /// shadow, and then `MARK`: a frame of its own, which counts against
    /// the rate cap as any frame does.
    async fn refresh(&mut self) -> Result<()> {
        let now = Instant::now();
        let due = self.lossy.due(now);
        if due.is_empty() {
            return Ok(());
        }
        self.next_frame = now + self.ctx.display.frame_interval;
        let options = self.encode_options().lossless();
        let region = due.clone();
        let started = Instant::now();
        let draws = self
            .with_encoder(move |encoder| encoder.refresh(&region, options))
            .await?
            .context("encoding a refresh")?
            .draws;
        self.conn
            .session()
            .counters()
            .encode_done(started.elapsed());
        debug!(
            session = self.conn.session().id(),
            tiles = due.tile_count(),
            draws = draws.len(),
            "sending lossy tiles again losslessly"
        );
        self.send_draws(&draws).await?;
        // Every tile of the region was sent losslessly, whether or not a
        // draw covered it wholly.
        self.lossy.clear(&due);
        if !draws.is_empty() {
            self.send(DisplayOpcode::Mark, &[]).await?;
            self.conn.session().counters().frame_sent(true);
        }
        Ok(())
    }

    /// Record a frame sent at `now`, which changed `changed`, of which
    /// `photographic` went out photographic, in the heat map, and start
    /// or end a stream as it says. Does nothing when streams may not be
    /// sent, or the frame changed nothing.
    async fn detect(
        &mut self,
        now: Instant,
        changed: &Region,
        photographic: &Region,
    ) -> Result<()> {
        if !self.streams.enabled() || changed.is_empty() {
            return Ok(());
        }
        self.heat.record(now, changed, photographic);
        let candidates = self.heat.candidates(now);
        match choose(&candidates, self.streams.live().map(Stream::rect)) {
            Choice::Start(rect) => self.start_stream(rect, now).await,
            Choice::End => {
                debug!(
                    session = self.conn.session().id(),
                    "a changing area has outgrown the video stream; ending it"
                );
                self.end_stream().await
            }
            Choice::Stay => Ok(()),
        }
    }

    /// Start a stream over `rect` at `now`, as [`Streams::start`] shapes
    /// it, and send its `STREAM_CREATE`, then its `STREAM_ACTIVATE_REPORT`
    /// if the client sends reports. Does nothing without a surface, or if
    /// no stream may start. If the encoder cannot be built, streams are
    /// turned off.
    async fn start_stream(&mut self, rect: Rectangle, now: Instant) -> Result<()> {
        let session = self.conn.session().id();
        let Some(size) = self.surface else {
            return Ok(());
        };
        let (create, activate, target) = match self.streams.start(rect, size, now) {
            Ok(Some(stream)) => (
                stream.create()?,
                stream.activate_report(),
                stream.target_bps(),
            ),
            Ok(None) => return Ok(()),
            Err(e) => return self.disable_streams(None, &format!("{e:#}")).await,
        };
        debug!(
            session,
            stream = create.id,
            width = create.stream_width,
            height = create.stream_height,
            x = create.dest.left,
            y = create.dest.top,
            target_kbps = target / 1000,
            "starting a video stream"
        );
        self.conn.session().counters().stream_created(target);
        self.send_msg(DisplayOpcode::StreamCreate, &create).await?;
        if let Some(activate) = activate {
            self.send_msg(DisplayOpcode::StreamActivateReport, &activate)
                .await?;
        }
        Ok(())
    }

    /// Whether pending damage would feed the live stream.
    fn stream_frame_due(&self) -> bool {
        self.streams
            .live()
            .is_some_and(|stream| self.damage.touches(stream.rect()))
    }

    /// Count a server drop against the live stream: a frame of it was due
    /// at `now` while the ack window was full. The next is counted a frame
    /// interval later at the soonest.
    async fn server_drop(&mut self, now: Instant) -> Result<()> {
        self.next_drop = now + self.ctx.display.frame_interval;
        let Some(stream) = self.streams.live_mut() else {
            return Ok(());
        };
        self.conn.session().counters().stream_server_drop();
        let changed = stream.server_drop(now);
        self.rate_changed(changed).await
    }

    /// Log a change of the live stream's bitrate, and record it in the
    /// metrics; or, if the encoder refused it, end the stream and turn
    /// streams off.
    async fn rate_changed(&mut self, changed: Result<Option<RateChange>>) -> Result<()> {
        match changed {
            Ok(None) => Ok(()),
            Ok(Some(change)) => {
                debug!(
                    session = self.conn.session().id(),
                    stream = ?self.streams.live().map(Stream::id),
                    from_kbps = change.from / 1000,
                    to_kbps = change.to / 1000,
                    reason = ?change.reason,
                    "the stream's bitrate changed"
                );
                self.conn.session().counters().stream_target(change.to);
                Ok(())
            }
            Err(e) => {
                let live = self.streams.end();
                self.disable_streams(live, &format!("{e:#}")).await
            }
        }
    }

    /// Capture the live stream's rectangle, bring the shadow up to date
    /// with it, and send it as one `STREAM_DATA` stamped with the
    /// session's time as the capture started. `damage` is the frame's,
    /// for a failed capture to put back. Returns the tiles that changed,
    /// or `None` if nothing did, or nothing was captured.
    ///
    /// A capture that changed nothing sends nothing. One that changed
    /// something leaves the stream's tiles lossy, due only after it would
    /// end, even if the encoder skips the frame to hold its bitrate: the
    /// next frame catches the client up, or the refresh at the end does.
    /// A capture that finds the desktop resized adds damage of it all, so
    /// that a frame replaces the surface, ending the stream. A failure of
    /// the encoder ends the stream and turns streams off.
    ///
    /// The shadow and the stream move into one task on the blocking pool
    /// with the capture, and back out with the result, so no stream is
    /// live meanwhile. If the task panics, both are lost with it and this
    /// fails, closing the channel.
    async fn stream_frame(&mut self, damage: &Region, now: Instant) -> Result<Option<Region>> {
        let Some(live) = self.streams.live() else {
            return Ok(None);
        };
        let (rect, size) = (live.rect(), damage.size());
        let covered = live.covered(size);
        let captured_at = self.conn.session().elapsed();
        let Some(capture) = self.capture_or_skip(Some(vec![rect])).await? else {
            let mut lost = Region::empty(size);
            for (col, row) in covered.tiles() {
                if damage.contains_tile(col, row) {
                    lost.insert_tile(col, row);
                }
            }
            self.damage.restore(&lost)?;
            return Ok(None);
        };
        if self.surface != Some(capture.size()) {
            // The desktop resized before its damage said so. Any damage
            // starts a frame, whose capture finds the new size.
            self.damage.add(&Region::full(capture.size()))?;
            return Ok(None);
        }
        let Some(mut stream) = self.streams.take() else {
            return Ok(None);
        };
        let mut encoder = std::mem::take(&mut self.encoder);
        let started = Instant::now();
        let (encoder, mut stream, result) = tokio::task::spawn_blocking(move || {
            let result = encoder.absorb(&capture).map(|changed| {
                let frame = (!changed.is_empty()).then(|| match capture.patches() {
                    [patch] => stream.encode(patch.pixels(), now),
                    patches => Err(anyhow::anyhow!(
                        "a capture of the stream's {rect} came back in {} patches",
                        patches.len()
                    )),
                });
                (changed, frame)
            });
            (encoder, stream, result)
        })
        .await
        .context("the stream's encoding task failed")?;
        self.encoder = encoder;
        let (changed, frame) = match result {
            Ok(result) => result,
            Err(e) => {
                self.streams.put_back(stream);
                return Err(e.context("bringing the shadow up to date with a stream's capture"));
            }
        };
        let Some(frame) = frame else {
            self.streams.put_back(stream);
            return Ok(None);
        };
        let counters = self.conn.session().counters();
        counters.stream_encode_done(started.elapsed());
        let held = now + STREAM_QUIET + self.ctx.display.lossless_delay;
        self.lossy.mark(&stream.touched(size), held);
        stream.changed(now);
        if let Ok(Encoded {
            frame_rate: Some(rate),
            ..
        }) = &frame
        {
            debug!(
                session = self.conn.session().id(),
                stream = stream.id(),
                frame_rate = rate,
                "the stream's frame rate changed"
            );
        }
        match frame.map(|encoded| encoded.frame) {
            Ok(Some(frame)) => {
                let data = stream.data(frame, captured_at);
                self.streams.put_back(stream);
                self.conn
                    .session()
                    .counters()
                    .stream_frame_sent(data.data.len() as u64);
                self.send_msg(DisplayOpcode::StreamData, &data).await?;
            }
            Ok(None) => {
                debug!(
                    session = self.conn.session().id(),
                    stream = stream.id(),
                    "the encoder skipped a frame to hold its bitrate"
                );
                self.streams.put_back(stream);
            }
            Err(e) => {
                self.disable_streams(Some(stream), &format!("encoding failed: {e:#}"))
                    .await?;
            }
        }
        Ok(Some(changed))
    }

    /// End the live stream if nothing has changed it for
    /// [`STREAM_QUIET`].
    async fn end_quiet_stream(&mut self) -> Result<()> {
        let quiet = self
            .streams
            .live()
            .is_some_and(|stream| stream.quiet_at() <= Instant::now());
        if !quiet {
            return Ok(());
        }
        debug!(
            session = self.conn.session().id(),
            "the video stream has been still; ending it"
        );
        self.end_stream().await
    }

    /// End the live stream, if there is one.
    async fn end_stream(&mut self) -> Result<()> {
        match self.streams.end() {
            Some(stream) => self.destroy_stream(stream).await,
            None => Ok(()),
        }
    }

    /// Tell the client `stream` has ended. Its tiles fall due at once, so
    /// the next refresh sends them losslessly, and their heat is
    /// forgotten.
    async fn destroy_stream(&mut self, stream: Stream) -> Result<()> {
        debug!(
            session = self.conn.session().id(),
            stream = stream.id(),
            "ending a video stream"
        );
        self.conn.session().counters().stream_destroyed();
        if let Some(size) = self.surface {
            let touched = stream.touched(size);
            self.lossy.make_due(&touched, Instant::now());
            self.heat.forget(&touched);
        }
        self.send_msg(DisplayOpcode::StreamDestroy, &stream.destroy())
            .await
    }

    /// Turn streams off for the rest of the channel, because of `why`,
    /// ending `stream`, which must no longer be live. Warns only the first
    /// time.
    async fn disable_streams(&mut self, stream: Option<Stream>, why: &str) -> Result<()> {
        if self.streams.disable() {
            warn!(
                session = self.conn.session().id(),
                stream = ?stream.as_ref().map(Stream::id),
                why,
                "a video stream failed; sending no more video streams on this connection"
            );
        }
        match stream {
            Some(stream) => self.destroy_stream(stream).await,
            None => Ok(()),
        }
    }

    /// Re-read the scrub's next slice of the desktop, and add each tile
    /// that differs from the shadow to the pending damage, for the next
    /// frame to send. Does nothing without a surface, without room in the
    /// ack window, or within [`CAPTURE_RETRY`] of a failed capture. A
    /// failed capture reads the same slice next time.
    ///
    /// The slices are full-width bands of whole tile rows, top to bottom
    /// and wrapping, each [`SCRUB_SLICES`]th of the desktop's tile rows
    /// rounded up; one is read each tick, so the whole desktop is read at
    /// least once every `scrub_period`.
    async fn scrub(&mut self) -> Result<()> {
        let Some(size) = self.surface else {
            return Ok(());
        };
        if !self.conn.may_send() || Instant::now() < self.capture_retry {
            return Ok(());
        }
        if self.scrub_top >= size.height {
            self.scrub_top = 0;
        }
        let height = scrub_slice_height(size.height);
        let slice = Rectangle::new(
            0,
            self.scrub_top,
            size.width,
            height.min(size.height - self.scrub_top),
        );
        let Some(capture) = self.capture_or_skip(Some(vec![slice])).await? else {
            return Ok(());
        };
        self.scrub_top = self.scrub_top.saturating_add(height);
        if capture.size() != size {
            // The desktop resized before its damage said so. Any damage
            // starts a frame, whose capture finds the new size.
            self.damage.add(&Region::full(size))?;
            return Ok(());
        }

        let differences = self
            .with_encoder(move |encoder| encoder.differences(&capture))
            .await?
            .context("comparing the scrub's slice")?;
        if differences.is_empty() {
            return Ok(());
        }
        // Tiles already pending are not counted: damage reported them.
        let added = self.damage.add(&differences)?;
        if added > 0 {
            let total = self.conn.session().counters().scrub_found(added as u64);
            debug!(
                session = self.conn.session().id(),
                slice = %slice,
                tiles = added,
                total,
                "the scrub found changes the desktop's damage did not report"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_slices_are_whole_tile_rows_and_cover_the_desktop() {
        // A desktop of 21 tile rows, the last partial: two rows a slice,
        // eleven slices.
        assert_eq!(scrub_slice_height(1308), 128);
        for height in [1, 64, 65, 192, 768, 1024, 1308, 2160, 16384] {
            let slice = scrub_slice_height(height);
            assert_eq!(slice % TILE, 0, "{height} rows");
            assert!(
                height.div_ceil(slice) <= SCRUB_SLICES,
                "{height} rows take more than {SCRUB_SLICES} slices of {slice}"
            );
        }
        // A small desktop is one tile row a slice.
        assert_eq!(scrub_slice_height(192), TILE);
    }

    #[test]
    fn the_scrub_ticks_sixteen_times_a_period() {
        assert_eq!(scrub_tick(SCRUB_PERIOD), Duration::from_millis(250));
        assert_eq!(scrub_tick(Duration::ZERO), Duration::from_millis(1));
    }
}
