//! The display channel.
//!
//! # The start sequence
//!
//! As spice-server's display channel does, andris sends `SET_ACK` (window
//! [`ACK_WINDOW`]) and then waits for the client's `INIT`. Andris keeps no
//! pixmap cache and no GLZ dictionary, so `INIT` is only validated. Then
//! it sends:
//!
//! 1. `SURFACE_CREATE` for the primary surface, 32-bit XRGB, at the
//!    desktop's size;
//! 2. a complete frame: all of the desktop, in as few `DRAW_COPY`s as
//!    fit (see [`updates`]);
//! 3. `MONITORS_CONFIG` with one head, if the client has that capability;
//! 4. `MARK`.
//!
//! `INVAL_ALL_PALETTES` is not sent: andris sends no palettes.
//!
//! # Frames
//!
//! Damage drives the display. The channel subscribes to the desktop's
//! damage once, before the client's `INIT`, and a new subscription starts
//! with the whole desktop pending, which the first frame takes. After that
//! nothing is captured until the platform reports damage.
//!
//! A frame starts only when all three hold:
//!
//! - damage is pending;
//! - at least `frame_interval` has passed since the last frame started:
//!   the interval is a cap on the frame rate, not a polling period;
//! - the ack window has room ([`ChannelConn::may_send`]).
//!
//! Damage reported while a frame cannot start stays pending in the
//! subscription, where it merges with what came before. Pixels are never
//! queued: when the frame starts it captures them fresh, so an
//! intermediate state the client was too slow to see is never sent.
//!
//! A frame takes the pending damage, captures its rectangles (at most
//! [`MAX_CAPTURE_RECTS`] of them) on the blocking pool, and compares them
//! with the shadow of what the client was last sent (see [`updates`]).
//! The tiles that changed are sent as `DRAW_COPY`s, each waiting for room
//! in the ack window, and then a `MARK`, which is what ryll counts as a
//! frame. Damage that turns out to have changed nothing sends nothing, not
//! even the `MARK`.
//!
//! The window fills while a client is behind, and only an `ACK` empties
//! it. `ACK`s are consumed inside the connection and never returned by
//! [`ChannelConn::recv`], so a channel waiting to start a frame for want of
//! room reads the client through [`ChannelConn::wait_for_room`] instead:
//! it returns as soon as an `ACK` makes room, and keeps any other message
//! for the next `recv`.
//!
//! # Resizes
//!
//! Pending damage is a region of the desktop's size as the platform last
//! reported it. A region of another size than the surface means the
//! desktop has resized, and so does a capture of another size, which
//! catches a resize the damage has not reported yet. Either way the
//! channel captures the whole desktop, destroys the surface, and re-runs
//! the start sequence from `SURFACE_CREATE` at the capture's size.
//!
//! The size is the capture's own, not a separate geometry query, so the
//! surface is always created at the size of the frame about to be drawn
//! on it, even when the desktop resizes between the two.
//!
//! # Failed captures
//!
//! A failed capture skips the frame rather than closing the channel: a
//! desktop resized while it is being captured (as RDP resizes an xrdp
//! session) fails captures for a while, and then they succeed. The damage
//! the frame took is put back, to be captured again.
//!
//! After any failed capture, a frame's or the scrub's, nothing is captured
//! again for the longer of `frame_interval` and [`CAPTURE_RETRY`]: no
//! frame starts, and the scrub skips its ticks. So the failures a channel
//! tolerates span a fixed time, about [`MAX_CAPTURE_FAILURES`] times
//! [`CAPTURE_RETRY`], however short the frame interval.
//!
//! The first failure is logged as a warning and the recovery as
//! information; a capture that succeeds resets the count. The channel
//! closes once [`MAX_CAPTURE_FAILURES`] captures in a row have failed,
//! counting the scrub's, or at once if the platform says the desktop is
//! gone for good.
//!
//! # Scrub
//!
//! Damage can miss a change. So [`SCRUB_SLICES`] times every
//! `scrub_period` the channel re-reads one slice of the desktop, a
//! full-width band of whole tile rows, and compares it with the shadow
//! without changing it.
//! Any tile that differs is added to the pending damage, and the next
//! frame captures and sends it as it would any other. The slices run top
//! to bottom and wrap. Each is [`SCRUB_SLICES`]th of the desktop's tile
//! rows, rounded up, so the whole desktop is read at least once every
//! `scrub_period` ([`SCRUB_PERIOD`] unless a test shortens it), and a
//! change damage missed reaches the client within about that long.
//!
//! The scrub runs only while the surface exists and the ack window has
//! room. A still desktop costs one slice's capture and comparison a tick,
//! and sends nothing.
//!
//! # What the client sends
//!
//! `PREFERRED_COMPRESSION` is honoured whenever it arrives: `OFF` means
//! uncompressed bitmaps, and anything else means LZ4 when the client can
//! decode it. `PREFERRED_VIDEO_CODEC_TYPE` and `STREAM_REPORT` are parsed
//! and ignored, since andris sends no streams. A malformed message closes
//! the channel; an unknown one is ignored.
//!
//! [`updates`]: super::updates

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

use super::channel::{ACK_WINDOW, ChannelConn, ChannelContext};
use super::opcodes::DisplayOpcode;
use super::updates::{EncodeOptions, SURFACE_ID, UpdateEncoder};
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
    let scrub_every = scrub_tick(ctx.scrub_period);
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
        scrub_tiles: 0,
    };

    // Wait for INIT, handling whatever else arrives first.
    while !display.initialised {
        let Some(message) = display.conn.recv().await? else {
            return Ok(());
        };
        display.handle(&message)?;
    }

    let mut scrub_ticks = tokio::time::interval_at(Instant::now() + scrub_every, scrub_every);
    scrub_ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let cap_open = Instant::now() >= display.next_frame;
        if display.damage_pending && cap_open && display.conn.may_send() {
            display.frame().await?;
            continue;
        }
        // Something blocks a frame, or nothing is pending. Wait for what
        // would unblock it, for the client, and for the scrub, but never
        // for damage already known to be pending: changed() would return
        // at once, and the loop would spin.
        let pending = display.damage_pending;
        let await_room = pending && cap_open;
        let next_frame = display.next_frame;
        // Every branch is cancel-safe: next_incoming wraps recv() or
        // wait_for_room(), each of which keeps whatever it read; changed()
        // loses nothing when dropped; a sleep is made afresh each time
        // round; and an interval's tick() is cancel-safe.
        tokio::select! {
            incoming = next_incoming(&mut display.conn, await_room) => match incoming? {
                Incoming::Message(message) => display.handle(&message)?,
                Incoming::Closed => return Ok(()),
                Incoming::Room => {}
            },
            () = display.damage.changed(), if !pending => display.damage_pending = true,
            () = tokio::time::sleep_until(next_frame), if pending && !cap_open => {}
            _ = scrub_ticks.tick() => display.scrub().await?,
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
    /// How many tiles the scrub has found changed that damage had not
    /// reported.
    scrub_tiles: u64,
}

impl Display {
    /// Handle one message from the client.
    fn handle(&mut self, message: &ReceivedMessage) -> Result<()> {
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
                PreferredVideoCodecType::decode(message.payload())
                    .context("a malformed PREFERRED_VIDEO_CODEC_TYPE")?;
                self.conn.ignore(message);
            }
            display_client::STREAM_REPORT => {
                StreamReport::decode(message.payload()).context("a malformed STREAM_REPORT")?;
                self.conn.ignore(message);
            }
            _ => self.conn.ignore(message),
        }
        Ok(())
    }

    /// How images may be encoded: LZ4 if the client can decode it and has
    /// not asked for no compression.
    fn encode_options(&self) -> EncodeOptions {
        EncodeOptions {
            lz4: self
                .ctx
                .caps
                .has_channel_mask(capabilities::DISPLAY_LZ4_COMPRESSION)
                && self.preferred_compression != Some(image_compression::OFF),
        }
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
        match self.capture(rects).await {
            Ok(capture) => {
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
                let retry = Instant::now() + self.ctx.frame_interval.max(CAPTURE_RETRY);
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
    async fn frame(&mut self) -> Result<()> {
        self.damage_pending = false;
        let region = self.damage.take().context("taking the desktop's damage")?;
        if region.is_empty() {
            return Ok(());
        }
        self.next_frame = Instant::now() + self.ctx.frame_interval;

        // Without a surface of the region's size, the surface is created
        // afresh, which needs all of the desktop.
        let whole = self.surface != Some(region.size());
        let rects = (!whole).then(|| region.rectangles(MAX_CAPTURE_RECTS));
        let Some(mut capture) = self.capture_or_skip(rects).await? else {
            return self.damage.restore(&region);
        };
        if !whole && self.surface != Some(capture.size()) {
            // The desktop resized before its damage said so.
            let Some(all) = self.capture_or_skip(None).await? else {
                return self.damage.restore(&region);
            };
            capture = all;
        }

        if self.surface != Some(capture.size()) {
            return self.replace_surface(capture).await;
        }
        if self.send_update(capture).await? > 0 {
            self.send(DisplayOpcode::Mark, &[]).await?;
        }
        Ok(())
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
        // shadow describes.
        self.encoder.reset();
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
        self.send(DisplayOpcode::Mark, &[]).await
    }

    /// Send what in `capture` differs from what the client was last
    /// sent: all of it after [`UpdateEncoder::reset`]. Returns how many
    /// draws were sent. Comparing and encoding are CPU-bound, so they run
    /// on the blocking pool.
    async fn send_update(&mut self, capture: Capture) -> Result<usize> {
        let options = self.encode_options();
        let mut encoder = std::mem::take(&mut self.encoder);
        let (encoder, update) = tokio::task::spawn_blocking(move || {
            let update = encoder.update(&capture, options);
            (encoder, update)
        })
        .await
        .context("the encoding task failed")?;
        self.encoder = encoder;
        let draws = update?.draws;
        for draw in &draws {
            self.send(DisplayOpcode::DrawCopy, &draw.body).await?;
        }
        Ok(draws.len())
    }

    /// Re-read the scrub's next slice of the desktop, and add each tile
    /// that differs from the shadow to the pending damage, for the next
    /// frame to send. Does nothing without a surface, without room in the
    /// ack window, or within [`CAPTURE_RETRY`] of a failed capture. A
    /// failed capture reads the same slice next time.
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

        let encoder = std::mem::take(&mut self.encoder);
        let (encoder, differences) = tokio::task::spawn_blocking(move || {
            let differences = encoder.differences(&capture);
            (encoder, differences)
        })
        .await
        .context("the scrub's comparing task failed")?;
        self.encoder = encoder;
        let differences = differences?;
        if differences.is_empty() {
            return Ok(());
        }
        // Tiles already pending are not counted: damage reported them.
        let added = self.damage.add(&differences)?;
        if added > 0 {
            self.scrub_tiles += added as u64;
            debug!(
                session = self.conn.session().id(),
                slice = %slice,
                tiles = added,
                total = self.scrub_tiles,
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
