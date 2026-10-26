//! The main channel.
//!
//! It opens the session with `INIT` and a `PING` (which ryll waits for
//! before it asks for the channel list), answers `ATTACH_CHANNELS` with
//! the channels andris serves, applies the client's mouse mode request,
//! and pings every [`PING_INTERVAL`] to keep the connection measured.
//! `INIT` says a guest agent is connected: andris plays the agent itself,
//! and hands `AGENT_START`, `AGENT_DATA` and `AGENT_TOKEN` to an
//! [`Agent`], which also follows the desktop's clipboard (see [`agent`]).
//! Everything else the client sends is read and ignored.
//!
//! [`agent`]: super::agent
//!
//! # A replaced session
//!
//! When a new client replaces the session, the main channel says goodbye
//! before it closes (see [`farewell`]): a `NOTIFY` whose text names the
//! client that took over, then `DISCONNECTING` with reason
//! `SPICE_LINK_ERR_OK`. spice-server never sends `DISCONNECTING`, so it
//! means only this: andris ended the session on purpose, and the client
//! should not reconnect by itself, which would take the desktop straight
//! back from the client that just took it. Only once both are written do
//! the session's other channels close, so the client hears why before it
//! sees any channel go. Until then they carry on, but the inputs channel
//! no longer passes the client's input to the desktop. A session that
//! ends any other way, such as andris shutting down, closes without
//! either message, so a client still reconnects after a restart.
//!
//! # Metrics
//!
//! This channel also reports the session's metrics (see [`stats`]). Its
//! task lives exactly as long as the session does: a session ends when
//! its main channel does, and ending a session cancels the task. So it
//! needs no task of its own and nothing to stop. Every
//! [`STATS_INTERVAL`] it logs a summary of the interval if any frame was
//! sent in it. A [`SessionSummary`] guard, which the server holds around
//! the channel, logs the whole session's once when the channel ends,
//! however it ends, with which channel closed first and why.
//!
//! [`stats`]: super::stats

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use shakenfist_spice_protocol::messages::{
    ChannelEntry, ChannelsList, Disconnecting, MainInit, MainMouseMode, MouseModeRequest, Notify,
    Ping, WireType,
};
use shakenfist_spice_protocol::{
    ChannelType, MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER, NotifySeverity, SpiceError, SpiceVisibility,
    main_client,
};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info};

use super::agent::{AGENT_WINDOW, Agent};
use super::channel::{ChannelConn, ChannelContext};
use super::opcodes::MainOpcode;
use super::session::{MouseMode, Session};
use super::stats::{Reporter, STATS_INTERVAL};

/// How often the main channel pings the client.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// How long a replaced session's main channel has to say goodbye and
/// close, before the server ends the session regardless.
pub const FAREWELL_TIMEOUT: Duration = Duration::from_secs(2);

/// `SPICE_WARN_GENERAL` (spice-protocol's `spice/enums.h`), the only
/// warning code SPICE defines: a `NOTIFY`'s `what` when its severity is
/// warn.
const SPICE_WARN_GENERAL: u32 = 0;

/// The channels `CHANNELS_LIST` offers, in order, when the platform has
/// audio; without it, [`ChannelType::Playback`] is left out (see
/// [`offered_channels`]). Inputs must come last: ryll stops reading the
/// list at the inputs channel (its session loop `break`s once the one
/// inputs channel has taken its event receiver), so any channel listed
/// after it is never opened (ryll#509). spice-server lists inputs after
/// the display and cursor channels, so against QEMU it goes unnoticed.
pub const CHANNELS: [ChannelType; 4] = [
    ChannelType::Display,
    ChannelType::Cursor,
    ChannelType::Playback,
    ChannelType::Inputs,
];

/// The channels `CHANNELS_LIST` offers: [`CHANNELS`], less playback
/// unless `audio`, since andris then refuses a playback link.
pub fn offered_channels(audio: bool) -> impl Iterator<Item = ChannelType> {
    CHANNELS
        .into_iter()
        .filter(move |&channel| audio || channel != ChannelType::Playback)
}

/// The mouse modes andris supports, as `SPICE_MOUSE_MODE_*` flags.
// Both constants are 1 and 2, so the sum fits a u16.
const SUPPORTED_MOUSE_MODES: u16 = (MOUSE_MODE_SERVER | MOUSE_MODE_CLIENT) as u16;

/// Serve a main channel until the client closes it.
pub async fn run(ctx: ChannelContext, conn: ChannelConn<MainOpcode>) -> Result<()> {
    serve_reporting(ctx, conn, PING_INTERVAL, STATS_INTERVAL).await
}

/// Logs the whole session's metrics when dropped: that is, when the main
/// channel's task ends or is cancelled, which is the session's end. It
/// is held outside [`drive`](super::channel::drive), so the line also
/// says which channel closed first, and why, even when that was main.
pub struct SessionSummary(Arc<Session>);

impl SessionSummary {
    pub fn new(session: Arc<Session>) -> Self {
        Self(session)
    }
}

impl Drop for SessionSummary {
    fn drop(&mut self) {
        let elapsed = self.0.elapsed();
        info!(
            session = self.0.id(),
            replaced_by = self.0.replaced_by().map(tracing::field::display),
            closed_first = self.0.closed_first().map(tracing::field::display),
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
    let mut agent = Agent::new(session.id(), &ctx.platform);

    let init = MainInit {
        session_id: session.id(),
        display_channels_hint: 1,
        supported_mouse_modes: u32::from(SUPPORTED_MOUSE_MODES),
        current_mouse_mode: session.mouse_mode().borrow().to_wire(),
        agent_connected: 1,
        agent_tokens: AGENT_WINDOW,
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
            // The client's messages and the desktop's clipboard are no
            // longer this client's business once it is replaced, and an arm
            // may be picked in the same poll in which that happens, so
            // those arms check again.
            () = session.replaced() => {
                return farewell(&mut conn, &session).await;
            }
            message = conn.recv() => {
                let Some(message) = message? else {
                    return Ok(());
                };
                if session.is_replaced() {
                    return farewell(&mut conn, &session).await;
                }
                match message.header.message_type {
                    main_client::ATTACH_CHANNELS => {
                        let list = ChannelsList {
                            channels: offered_channels(ctx.platform.audio.is_some())
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
                    main_client::AGENT_START => agent.start(&mut conn, message.payload()).await?,
                    main_client::AGENT_DATA => agent.data(&mut conn, message.payload()).await?,
                    main_client::AGENT_TOKEN => agent.token(&mut conn, message.payload()).await?,
                    _ => conn.ignore(&message),
                }
            }
            // Agent tokens go back only once andris has handled all it has
            // read, so a client that sends past its window is found out.
            () = std::future::ready(()), if agent.owes_tokens() && !conn.has_unread() => {
                agent.return_tokens(&mut conn).await?;
            }
            () = agent.desktop_clipboard_changed() => {
                if session.is_replaced() {
                    return farewell(&mut conn, &session).await;
                }
                agent.desktop_clipboard(&mut conn).await?;
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

/// Tell a replaced session's client why its session is ending, and close
/// the channel. The server bounds this by [`FAREWELL_TIMEOUT`].
async fn farewell(conn: &mut ChannelConn<MainOpcode>, session: &Session) -> Result<()> {
    let by = session
        .replaced_by()
        .context("a session ends for its replacement without one")?;
    debug!(session = session.id(), %by, "telling the replaced client why");
    conn.send_msg(MainOpcode::Notify, &replaced_notify(session.clock_ns(), by))
        .await?;
    let disconnecting = Disconnecting {
        timestamp: session.clock_ns(),
        reason: SpiceError::Ok as u32,
    };
    conn.send_msg(MainOpcode::Disconnecting, &disconnecting)
        .await?;
    session.close_secondary_channels();
    conn.close().await
}

/// The `NOTIFY` that tells a client another, from `by`, has taken its
/// session over. It names only the address, not the port: andris's log
/// has both.
fn replaced_notify(timestamp: u64, by: SocketAddr) -> Notify {
    Notify {
        timestamp,
        severity: NotifySeverity::Warn as u32,
        visibility: SpiceVisibility::High as u32,
        what: SPICE_WARN_GENERAL,
        message: format!("another client ({}) took over this session", by.ip()).into_bytes(),
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
    use shakenfist_spice_protocol::messages::{ReceiveBuffer, ReceivedMessage, make_message};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::platform::Size;
    use crate::platform::synthetic::SyntheticPlatform;
    use crate::server::channel::ClientCaps;
    use crate::server::display::DisplayOptions;
    use crate::server::opcodes::ServerOpcode;
    use crate::server::session::SessionRegistry;

    #[test]
    fn playback_is_offered_before_inputs_and_only_with_audio() {
        let with: Vec<_> = offered_channels(true).collect();
        assert_eq!(with, CHANNELS);
        assert_eq!(with.last(), Some(&ChannelType::Inputs));
        let without: Vec<_> = offered_channels(false).collect();
        assert_eq!(
            without,
            [
                ChannelType::Display,
                ChannelType::Cursor,
                ChannelType::Inputs
            ]
        );
    }

    /// Pings repeat at the interval given, with increasing ids, and
    /// carry the session clock.
    #[tokio::test]
    async fn pings_repeat_with_increasing_ids() {
        let platform = SyntheticPlatform::new(Size {
            width: 16,
            height: 16,
        })
        .unwrap();
        let session = SessionRegistry::new()
            .begin("192.0.2.1:5900".parse().unwrap())
            .unwrap();
        let ctx = ChannelContext {
            session: session.clone(),
            platform: platform.platform(),
            caps: ClientCaps::default(),
            channel_id: 0,
            display: DisplayOptions::default(),
        };
        let (server_end, mut client_end) = tokio::io::duplex(64 * 1024);
        let conn = ChannelConn::<MainOpcode>::new(server_end, session);
        let task = tokio::spawn(serve(ctx, conn, Duration::from_millis(20)));

        let mut buf = ReceiveBuffer::new();
        let mut ping_ids = Vec::new();
        let mut last_timestamp = 0;
        while ping_ids.len() < 4 {
            let message = loop {
                if let Some(message) = buf.take_message(1 << 20).unwrap() {
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

    const WAIT: Duration = Duration::from_secs(10);

    /// A main channel served over a pipe that holds `capacity` bytes.
    struct Served {
        _platform: Arc<SyntheticPlatform>,
        registry: SessionRegistry,
        session: Arc<Session>,
        client: DuplexStream,
        buf: ReceiveBuffer,
        task: JoinHandle<Result<()>>,
    }

    impl Served {
        fn start(capacity: usize) -> Self {
            let platform = SyntheticPlatform::new(Size {
                width: 16,
                height: 16,
            })
            .unwrap();
            let registry = SessionRegistry::new();
            let session = registry.begin("192.0.2.1:5900".parse().unwrap()).unwrap();
            let ctx = ChannelContext {
                session: session.clone(),
                platform: platform.platform(),
                caps: ClientCaps::default(),
                channel_id: 0,
                display: DisplayOptions::default(),
            };
            let (server_end, client) = tokio::io::duplex(capacity);
            let conn = ChannelConn::<MainOpcode>::new(server_end, session.clone());
            let task = tokio::spawn(serve(ctx, conn, Duration::from_secs(3600)));
            Self {
                _platform: platform,
                registry,
                session,
                client,
                buf: ReceiveBuffer::new(),
                task,
            }
        }

        async fn recv(&mut self) -> ReceivedMessage {
            loop {
                if let Some(message) = self.buf.take_message(1 << 20).unwrap() {
                    return message;
                }
                let mut chunk = [0u8; 4096];
                let read = tokio::time::timeout(WAIT, self.client.read(&mut chunk))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(read > 0, "the channel closed");
                self.buf.extend_from_slice(&chunk[..read]);
            }
        }

        /// Read `INIT` and the first `PING`.
        async fn skip_opening(&mut self) {
            assert_eq!(
                self.recv().await.header.message_type,
                MainOpcode::Init.code()
            );
            assert_eq!(
                self.recv().await.header.message_type,
                MainOpcode::Ping.code()
            );
        }

        fn replace(&self) {
            self.registry
                .begin("192.0.2.2:5900".parse().unwrap())
                .unwrap();
        }

        async fn secondary_closed_within(&self, wait: Duration) -> bool {
            tokio::time::timeout(wait, self.session.secondary_channels_closed())
                .await
                .is_ok()
        }
    }

    /// The other channels close only once the goodbye is written: a pipe
    /// too small for a whole message holds each write until the client
    /// reads it, and until then the other channels stay open.
    #[tokio::test]
    async fn other_channels_close_only_after_the_farewell_is_written() {
        let mut served = Served::start(16);
        served.skip_opening().await;
        served.replace();
        let pause = Duration::from_millis(50);
        assert!(
            !served.secondary_closed_within(pause).await,
            "closed before NOTIFY"
        );
        assert_eq!(
            served.recv().await.header.message_type,
            MainOpcode::Notify.code()
        );
        assert!(
            !served.secondary_closed_within(pause).await,
            "closed before DISCONNECTING"
        );
        assert_eq!(
            served.recv().await.header.message_type,
            MainOpcode::Disconnecting.code()
        );
        assert!(served.secondary_closed_within(WAIT).await);
        drop(served.client);
        tokio::time::timeout(WAIT, served.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// A message already waiting when the session is replaced is not
    /// acted on, though the channel's select may pick it in the same poll
    /// as the replacement. Repeated, since that pick is random.
    #[tokio::test]
    async fn a_message_waiting_at_the_replacement_is_not_acted_on() {
        let mut body = Vec::new();
        MouseModeRequest {
            mode: MOUSE_MODE_SERVER as u16,
        }
        .write(&mut body);
        let request = make_message(main_client::MOUSE_MODE_REQUEST, &body);
        for _ in 0..32 {
            let mut served = Served::start(64 * 1024);
            served.skip_opening().await;
            // Neither the write nor the replacement yields, so the channel
            // finds both ready when it next runs.
            served.client.write_all(&request).await.unwrap();
            served.replace();
            assert_eq!(
                served.recv().await.header.message_type,
                MainOpcode::Notify.code()
            );
            assert_eq!(*served.session.mouse_mode().borrow(), MouseMode::Client);
        }
    }
}
