//! The main channel.
//!
//! It opens the session with `INIT` and a `PING` (which ryll waits for
//! before it asks for the channel list), answers `ATTACH_CHANNELS` with
//! the channels andris serves, applies the client's mouse mode request,
//! and pings every [`PING_INTERVAL`] to keep the connection measured.
//! Everything else the client sends is read and ignored.
//!
//! # Metrics
//!
//! This channel also reports the session's metrics (see [`stats`]). Its
//! task lives exactly as long as the session does: a session ends when
//! its main channel does, and ending a session cancels the task. So it
//! needs no task of its own and nothing to stop. Every
//! [`STATS_INTERVAL`] it logs a summary of the interval if any frame was
//! sent in it, and a [`SessionSummary`] guard logs the whole session's
//! once when the task ends, however it ends.
//!
//! [`stats`]: super::stats

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use shakenfist_spice_protocol::messages::{
    ChannelEntry, ChannelsList, MainInit, MainMouseMode, MouseModeRequest, Ping, WireType,
};
use shakenfist_spice_protocol::{ChannelType, MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER, main_client};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info};

use super::channel::{ChannelConn, ChannelContext};
use super::opcodes::MainOpcode;
use super::session::{MouseMode, Session};
use super::stats::{Reporter, STATS_INTERVAL};

/// How often the main channel pings the client.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// The channels `CHANNELS_LIST` offers, in order. Inputs must come last:
/// ryll stops reading the list at the inputs channel (its session loop
/// `break`s once the one inputs channel has taken its event receiver), so
/// any channel listed after it is never opened (ryll#509). spice-server
/// lists inputs after the display and cursor channels, so against QEMU it
/// goes unnoticed.
pub const CHANNELS: [ChannelType; 3] = [
    ChannelType::Display,
    ChannelType::Cursor,
    ChannelType::Inputs,
];

/// The mouse modes andris supports, as `SPICE_MOUSE_MODE_*` flags.
// Both constants are 1 and 2, so the sum fits a u16.
const SUPPORTED_MOUSE_MODES: u16 = (MOUSE_MODE_SERVER | MOUSE_MODE_CLIENT) as u16;

/// Serve a main channel until the client closes it.
pub async fn run(ctx: ChannelContext, conn: ChannelConn<MainOpcode>) -> Result<()> {
    serve_reporting(ctx, conn, PING_INTERVAL, STATS_INTERVAL).await
}

/// Logs the whole session's metrics when dropped: that is, when the main
/// channel's task ends or is cancelled, which is the session's end.
struct SessionSummary(Arc<Session>);

impl Drop for SessionSummary {
    fn drop(&mut self) {
        let elapsed = self.0.elapsed();
        info!(
            session = self.0.id(),
            "session ends after {:.1} s: {}; {} kB written in all",
            elapsed.as_secs_f64(),
            self.0.stats().summary(elapsed),
            self.0.stats().total_bytes() / 1000,
        );
    }
}

/// Serve a main channel, pinging every `ping_interval`.
pub async fn serve(
    ctx: ChannelContext,
    conn: ChannelConn<MainOpcode>,
    ping_interval: Duration,
) -> Result<()> {
    serve_reporting(ctx, conn, ping_interval, STATS_INTERVAL).await
}

/// Serve a main channel, pinging every `ping_interval` and logging the
/// session's metrics every `stats_interval`.
pub async fn serve_reporting(
    ctx: ChannelContext,
    mut conn: ChannelConn<MainOpcode>,
    ping_interval: Duration,
    stats_interval: Duration,
) -> Result<()> {
    let session = ctx.session.clone();
    let _summary = SessionSummary(session.clone());

    let init = MainInit {
        session_id: session.id(),
        display_channels_hint: 1,
        supported_mouse_modes: u32::from(SUPPORTED_MOUSE_MODES),
        current_mouse_mode: session.mouse_mode().borrow().to_wire(),
        agent_connected: 0,
        agent_tokens: 0,
        // Milliseconds, wrapping: the field is a u32.
        multi_media_time: session.elapsed().as_millis() as u32,
        ram_hint: 0,
    };
    conn.send_msg(MainOpcode::Init, &init).await?;

    let mut ping_id: u32 = 1;
    send_ping(&ctx, &mut conn, ping_id).await?;

    let mut pings = tokio::time::interval_at(Instant::now() + ping_interval, ping_interval);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut reports = tokio::time::interval_at(Instant::now() + stats_interval, stats_interval);
    reports.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut reporter = Reporter::new(session.stats());
    let mut reported = Instant::now();

    loop {
        tokio::select! {
            message = conn.recv() => {
                let Some(message) = message? else {
                    return Ok(());
                };
                match message.header.message_type {
                    main_client::ATTACH_CHANNELS => {
                        let list = ChannelsList {
                            channels: CHANNELS
                                .into_iter()
                                .map(|channel_type| ChannelEntry {
                                    channel_type: channel_type as u8,
                                    channel_id: 0,
                                })
                                .collect(),
                        };
                        conn.send_msg(MainOpcode::ChannelsList, &list).await?;
                    }
                    main_client::MOUSE_MODE_REQUEST => {
                        let request = MouseModeRequest::decode(message.payload())
                            .context("a malformed MOUSE_MODE_REQUEST")?;
                        match MouseMode::from_wire(u32::from(request.mode)) {
                            Some(mode) => {
                                session.mouse_mode().send_replace(mode);
                            }
                            None => debug!(
                                session = session.id(),
                                mode = request.mode,
                                "unsupported mouse mode requested; unchanged"
                            ),
                        }
                        let mode = MainMouseMode {
                            supported_modes: SUPPORTED_MOUSE_MODES,
                            current_mode: session.mouse_mode().borrow().to_wire_u16(),
                        };
                        conn.send_msg(MainOpcode::MouseMode, &mode).await?;
                    }
                    _ => conn.ignore(&message),
                }
            }
            _ = reports.tick() => {
                let now = Instant::now();
                if let Some(line) = reporter.interval_line(session.stats(), now - reported) {
                    info!(session = session.id(), "metrics: {line}");
                }
                reported = now;
            }
            _ = pings.tick() => {
                ping_id = ping_id.wrapping_add(1);
                send_ping(&ctx, &mut conn, ping_id).await?;
            }
        }
    }
}

/// Send `PING` `id`, stamped with the session clock so the `PONG` says
/// when it left.
async fn send_ping(
    ctx: &ChannelContext,
    conn: &mut ChannelConn<MainOpcode>,
    id: u32,
) -> Result<()> {
    let ping = Ping {
        id,
        timestamp: ctx.session.clock_ns(),
        padding_len: 0,
    };
    conn.send_msg(MainOpcode::Ping, &ping).await
}

#[cfg(test)]
mod tests {
    use shakenfist_spice_protocol::messages::take_message;
    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::platform::Size;
    use crate::platform::synthetic::SyntheticPlatform;
    use crate::server::channel::ClientCaps;
    use crate::server::opcodes::ServerOpcode;
    use crate::server::session::SessionRegistry;

    /// Pings repeat at the interval given, with increasing ids, and
    /// carry the session clock.
    #[tokio::test]
    async fn pings_repeat_with_increasing_ids() {
        let platform = SyntheticPlatform::new(Size {
            width: 16,
            height: 16,
        })
        .unwrap();
        let session = SessionRegistry::new().begin().unwrap();
        let ctx = ChannelContext {
            session: session.clone(),
            platform: platform.platform(),
            caps: ClientCaps::default(),
            channel_id: 0,
            frame_interval: Duration::from_millis(50),
            scrub_period: Duration::from_secs(4),
            lossless_delay: Duration::from_millis(500),
            jpeg_quality: None,
        };
        let (server_end, mut client_end) = tokio::io::duplex(64 * 1024);
        let conn = ChannelConn::<MainOpcode>::new(server_end, session);
        let task = tokio::spawn(serve(ctx, conn, Duration::from_millis(20)));

        let mut buf = Vec::new();
        let mut ping_ids = Vec::new();
        let mut last_timestamp = 0;
        while ping_ids.len() < 4 {
            let message = loop {
                if let Some(message) = take_message(&mut buf, 1 << 20).unwrap() {
                    break message;
                }
                let mut chunk = [0u8; 4096];
                let read =
                    tokio::time::timeout(Duration::from_secs(10), client_end.read(&mut chunk))
                        .await
                        .unwrap()
                        .unwrap();
                assert!(read > 0, "the channel closed");
                buf.extend_from_slice(&chunk[..read]);
            };
            if message.header.message_type == MainOpcode::Ping.code() {
                let ping = Ping::decode(message.payload()).unwrap();
                assert!(ping.timestamp >= last_timestamp);
                last_timestamp = ping.timestamp;
                ping_ids.push(ping.id);
            }
        }
        assert_eq!(ping_ids, vec![1, 2, 3, 4]);
        task.abort();
    }
}
