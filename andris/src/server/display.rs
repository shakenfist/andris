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
//! 2. a complete frame, as one `DRAW_COPY` per band (see [`frames`]);
//! 3. `MONITORS_CONFIG` with one head, if the client has that capability;
//! 4. `MARK`.
//!
//! `INVAL_ALL_PALETTES` is not sent: andris sends no palettes.
//!
//! # Frames
//!
//! Every `frame_interval` andris captures the desktop and sends the bands
//! that changed. A tick is skipped, not queued, while more than twice the
//! ack window is unacknowledged, and every message waits for room in the
//! window before it is sent. If the capture is a different size from the
//! surface, the surface is destroyed and the start sequence re-run from
//! `SURFACE_CREATE` at the new size.
//!
//! The size is the capture's own, not a separate geometry query, so the
//! surface is always created at the size of the frame about to be drawn
//! on it, even when the desktop resizes between the two.
//!
//! # What the client sends
//!
//! `PREFERRED_COMPRESSION` is honoured whenever it arrives: `OFF` means
//! uncompressed bitmaps, and anything else means LZ4 when the client can
//! decode it. `PREFERRED_VIDEO_CODEC_TYPE` and `STREAM_REPORT` are parsed
//! and ignored, since andris sends no streams. A malformed message closes
//! the channel; an unknown one is ignored.
//!
//! [`frames`]: super::frames

use anyhow::{Context, Result, bail};
use shakenfist_spice_protocol::constants::{image_compression, surface_fmt};
use shakenfist_spice_protocol::messages::{
    DisplayHead, DisplayInit, DisplayMonitorsConfig, PreferredCompression, PreferredVideoCodecType,
    ReceivedMessage, StreamReport, SurfaceCreate, SurfaceDestroy, WireType,
};
use shakenfist_spice_protocol::{capabilities, display_client};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::debug;

use super::channel::{ACK_WINDOW, ChannelConn, ChannelContext};
use super::frames::{FrameEncoder, SURFACE_ID};
use super::opcodes::DisplayOpcode;
use crate::platform::{Frame, Size};

/// `SPICE_SURFACE_FLAGS_PRIMARY` (spice-protocol enums.h), which ryll's
/// protocol crate does not name: the surface is the display itself.
pub const SURFACE_FLAGS_PRIMARY: u32 = 1 << 0;

/// Serve a display channel until the client closes it.
pub async fn run(ctx: ChannelContext, mut conn: ChannelConn<DisplayOpcode>) -> Result<()> {
    conn.start_acks(ACK_WINDOW).await?;
    let mut display = Display {
        ctx,
        conn,
        preferred_compression: None,
        initialised: false,
        surface: None,
        encoder: FrameEncoder::new(),
    };

    // Wait for INIT, handling whatever else arrives first.
    while !display.initialised {
        let Some(message) = display.conn.recv().await? else {
            return Ok(());
        };
        display.handle(&message)?;
    }

    let frame = display.capture().await?;
    display.start_surface(frame).await?;

    let interval = display.ctx.frame_interval;
    let mut ticks = tokio::time::interval_at(Instant::now() + interval, interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            message = display.conn.recv() => {
                let Some(message) = message? else {
                    return Ok(());
                };
                display.handle(&message)?;
            }
            _ = ticks.tick() => {
                if display.conn.may_send() {
                    display.tick().await?;
                }
            }
        }
    }
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
    encoder: FrameEncoder,
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

    /// Whether bands may be LZ4: the client can decode it and has not
    /// asked for no compression.
    fn lz4_allowed(&self) -> bool {
        self.ctx
            .caps
            .has_channel_mask(capabilities::DISPLAY_LZ4_COMPRESSION)
            && self.preferred_compression != Some(image_compression::OFF)
    }

    /// Wait for room in the ack window, then send one message.
    async fn send(&mut self, op: DisplayOpcode, body: &[u8]) -> Result<()> {
        self.conn.wait_for_room().await?;
        self.conn.send(op, body).await
    }

    /// [`Display::send`] for a typed body.
    async fn send_msg<T: WireType>(&mut self, op: DisplayOpcode, message: &T) -> Result<()> {
        let mut body = Vec::new();
        message.write(&mut body);
        self.send(op, &body).await
    }

    /// Capture the desktop. Captures may block, so this runs on the
    /// blocking pool. It borrows `self` only to start, so the future
    /// stays `Send` although the connection is not `Sync`.
    fn capture(&self) -> impl Future<Output = Result<Frame>> + Send + 'static {
        let frames = self.ctx.platform.frames.clone();
        async move {
            tokio::task::spawn_blocking(move || frames.capture())
                .await
                .context("the capture task failed")?
                .context("capturing the desktop")
        }
    }

    /// Capture, and send what changed. A capture of another size than
    /// the surface replaces the surface.
    async fn tick(&mut self) -> Result<()> {
        let frame = self.capture().await?;
        if self.surface == Some(frame.size()) {
            return self.send_frame(frame, false).await;
        }
        if let Some(old) = self.surface.take() {
            debug!(
                session = self.conn.session().id(),
                from = %old,
                to = %frame.size(),
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
        self.start_surface(frame).await
    }

    /// Create the primary surface at `frame`'s size and draw all of it,
    /// then describe the monitor and mark the display ready.
    async fn start_surface(&mut self, frame: Frame) -> Result<()> {
        let size = frame.size();
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
        self.send_frame(frame, true).await?;
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

    /// Send `frame`'s bands: all of them if `complete`, otherwise those
    /// that changed. Encoding is CPU-bound, so it runs on the blocking
    /// pool.
    async fn send_frame(&mut self, frame: Frame, complete: bool) -> Result<()> {
        let lz4 = self.lz4_allowed();
        let mut encoder = std::mem::take(&mut self.encoder);
        let (encoder, bodies) = tokio::task::spawn_blocking(move || {
            let bodies = encoder.encode(frame, complete, lz4);
            (encoder, bodies)
        })
        .await
        .context("the encoding task failed")?;
        self.encoder = encoder;
        for body in bodies? {
            self.send(DisplayOpcode::DrawCopy, &body).await?;
        }
        Ok(())
    }
}
