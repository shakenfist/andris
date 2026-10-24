//! One linked channel: its socket, message framing, the messages every
//! channel handles alike, and SPICE's ack window.
//!
//! # The task model
//!
//! Each channel runs in one task, which owns its [`ChannelConn`] and so
//! the whole socket. A handler loops on `tokio::select!` over
//! [`ChannelConn::recv`] (which is cancel-safe) and its own timers or
//! watch receivers, and sends from the select arms. Nothing else writes
//! to the socket: the messages handled for every channel here (`PONG`,
//! `ACK_SYNC`, `ACK`, `DISCONNECTING`) need no reply.
//!
//! [`drive`] races the handler against its session's cancellation. When
//! the session is cancelled the handler's future is dropped, and the
//! socket with it, so cleanup a handler owes the desktop (releasing held
//! keys, say) belongs in a `Drop` guard: that runs on every exit path.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use shakenfist_spice_protocol::main_client;
use shakenfist_spice_protocol::messages::{
    AckSync, Disconnecting, Pong, ReceiveBuffer, ReceivedMessage, SetAck, WireType, make_message,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::debug;

use super::display::DisplayOptions;
use super::opcodes::{AckedOpcode, ServerOpcode};
use super::session::Session;
use crate::platform::Platform;

/// The ack window display and cursor ask for, as spice-server does.
pub const ACK_WINDOW: u32 = 20;

/// How long one message may take to write before the channel is given
/// up as stalled.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The most client messages [`ChannelConn::wait_for_room`] holds for the
/// handler while it waits for `ACK`s. A client that sends more than this
/// while withholding `ACK`s is misbehaving, and the channel closes.
pub const MAX_PENDING: usize = 64;

/// How much the receive buffer grows by for each read.
const READ_CHUNK: usize = 16 * 1024;

/// The longest `PING` round trip recorded. The round trip is taken from
/// the timestamp the client echoes, so a longer one, like one from a
/// timestamp in the future, says only that the client sent a timestamp
/// andris never did, and is discarded rather than skewing the metrics.
pub const MAX_PLAUSIBLE_RTT: Duration = Duration::from_secs(60);

/// The round trip of a `PING` stamped `sent_ns` whose `PONG` arrived at
/// `now_ns`, both on the session clock, or `None` if it is implausible:
/// sent in the future, or longer than [`MAX_PLAUSIBLE_RTT`].
fn round_trip(now_ns: u64, sent_ns: u64) -> Option<Duration> {
    let rtt = Duration::from_nanos(now_ns.checked_sub(sent_ns)?);
    (rtt <= MAX_PLAUSIBLE_RTT).then_some(rtt)
}

/// A byte stream a channel runs over: TCP, TLS, or an in-memory pipe in
/// tests.
pub trait ChannelStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> ChannelStream for T {}

/// The capability words a client advertised in its link message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientCaps {
    pub common: Vec<u32>,
    pub channel: Vec<u32>,
}

impl ClientCaps {
    fn has_bit(words: &[u32], bit: u32) -> bool {
        words
            .get((bit / 32) as usize)
            .is_some_and(|word| word & (1 << (bit % 32)) != 0)
    }

    /// Whether common capability number `bit` is set; bit 32 is bit 0 of
    /// the second word.
    pub fn has_common_cap(&self, bit: u32) -> bool {
        Self::has_bit(&self.common, bit)
    }

    /// Whether channel capability number `bit` is set; bit 32 is bit 0 of
    /// the second word.
    pub fn has_channel_cap(&self, bit: u32) -> bool {
        Self::has_bit(&self.channel, bit)
    }

    /// Whether every bit of `mask` is set in the first channel
    /// capability word, the form ryll's `capabilities` constants take.
    pub fn has_channel_mask(&self, mask: u32) -> bool {
        self.channel.first().is_some_and(|word| word & mask == mask)
    }
}

/// What a channel handler is given besides its connection.
#[derive(Clone)]
pub struct ChannelContext {
    /// The session the channel belongs to.
    pub session: Arc<Session>,
    /// The desktop.
    pub platform: Platform,
    /// What the client advertised when it linked this channel.
    pub caps: ClientCaps,
    /// The channel id the client linked; always 0 today.
    pub channel_id: u8,
    /// How a display channel paces its frames, and whether it may send
    /// JPEG.
    pub display: DisplayOptions,
}

impl ChannelContext {
    /// Return once the session has been cancelled. Cancel-safe. A handler
    /// need not watch for this: [`drive`] drops it when it happens.
    pub async fn cancelled(&self) {
        self.session.cancelled().await
    }
}

/// Run `handler` on `conn` until it returns or the session is
/// cancelled, then drop the connection, closing the socket.
pub async fn drive<O, F, Fut>(ctx: ChannelContext, conn: ChannelConn<O>, handler: F)
where
    O: ServerOpcode,
    F: FnOnce(ChannelContext, ChannelConn<O>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let session = ctx.session.clone();
    let channel = O::CHANNEL.name();
    debug!(session = session.id(), channel, "channel open");
    tokio::select! {
        biased;
        () = session.cancelled() => {
            debug!(session = session.id(), channel, "session ended; closing the channel");
        }
        result = handler(ctx, conn) => match result {
            Ok(()) => debug!(session = session.id(), channel, "channel closed"),
            Err(e) => debug!(
                session = session.id(),
                channel,
                error = format!("{e:#}"),
                "channel closed with an error"
            ),
        },
    }
}

/// SPICE's ack window for one channel, as the server keeps it.
///
/// The server counts the messages it sends from `SET_ACK` on, as ryll
/// counts what it receives. Each `ACK` from the client says it has
/// received another window's worth, once its `ACK_SYNC` has named the
/// current generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckWindow {
    generation: u32,
    window: u32,
    synced: bool,
    unacked: u64,
    /// Messages sent since `SET_ACK`.
    total_sent: u64,
    /// When each message that completed a window was sent, for the
    /// `ACK`s still to come.
    window_ends: VecDeque<Instant>,
}

impl AckWindow {
    fn new(generation: u32, window: u32) -> Self {
        Self {
            generation,
            window,
            synced: false,
            unacked: 0,
            total_sent: 0,
            window_ends: VecDeque::new(),
        }
    }

    fn sent(&mut self) {
        self.unacked = self.unacked.saturating_add(1);
        self.total_sent = self.total_sent.saturating_add(1);
        if self.window != 0 && self.total_sent.is_multiple_of(u64::from(self.window)) {
            self.window_ends.push_back(Instant::now());
        }
    }

    fn ack_sync(&mut self, generation: u32) {
        if generation == self.generation {
            self.synced = true;
        } else {
            debug!(
                got = generation,
                want = self.generation,
                "ACK_SYNC names another generation; ignored"
            );
        }
    }

    /// Account for an `ACK`. Returns how long ago the message that
    /// completed the window it acknowledges was sent.
    fn ack(&mut self) -> Option<Duration> {
        if !self.synced {
            return None;
        }
        self.unacked = self.unacked.saturating_sub(u64::from(self.window));
        self.window_ends.pop_front().map(|sent| sent.elapsed())
    }

    /// False while more than twice the window is unacknowledged.
    fn may_send(&self) -> bool {
        self.unacked <= 2 * u64::from(self.window)
    }
}

/// One message read from the client.
enum Event {
    /// A message for the channel's handler.
    Channel(ReceivedMessage),
    /// A common message, already handled.
    Common,
    /// The client closed the channel or sent `DISCONNECTING`.
    Closed,
}

/// What [`ChannelConn::handle_common`] did with a message.
enum Common {
    /// It was a common message, now handled.
    Handled,
    /// The client is disconnecting.
    Close,
    /// It is the channel's own message.
    Channel,
}

/// A linked, authenticated channel: its socket, receive buffer and ack
/// window. `O` is the channel's opcode enum, so it can send only that
/// channel's messages.
pub struct ChannelConn<O: ServerOpcode> {
    stream: Box<dyn ChannelStream>,
    buf: ReceiveBuffer,
    pending: VecDeque<ReceivedMessage>,
    ack: Option<AckWindow>,
    session: Arc<Session>,
    closed: bool,
    ignored: HashSet<u16>,
    _opcode: PhantomData<O>,
}

impl<O: ServerOpcode> ChannelConn<O> {
    pub fn new(stream: impl ChannelStream + 'static, session: Arc<Session>) -> Self {
        Self {
            stream: Box::new(stream),
            buf: ReceiveBuffer::new(),
            pending: VecDeque::new(),
            ack: None,
            session,
            closed: false,
            ignored: HashSet::new(),
            _opcode: PhantomData,
        }
    }

    /// Frame `body` as an `op` message and write it. Not cancel-safe: a
    /// partial write leaves the stream mid-message, so call it from a
    /// select arm's body, not as a branch.
    pub async fn send(&mut self, op: O, body: &[u8]) -> Result<()> {
        ensure!(
            u32::try_from(body.len()).is_ok(),
            "a {op:?} body of {} bytes is too long to frame",
            body.len()
        );
        let message = make_message(op.code(), body);
        let stream = &mut self.stream;
        tokio::time::timeout(WRITE_TIMEOUT, async {
            stream.write_all(&message).await?;
            stream.flush().await
        })
        .await
        .with_context(|| format!("writing {op:?} timed out"))?
        .with_context(|| format!("writing {op:?}"))?;
        if let Some(ack) = &mut self.ack {
            ack.sent();
        }
        self.session
            .counters()
            .bytes_sent(O::CHANNEL, message.len() as u64);
        Ok(())
    }

    /// Write `message` as an `op` message.
    pub async fn send_msg<T: WireType>(&mut self, op: O, message: &T) -> Result<()> {
        let mut body = Vec::new();
        message.write(&mut body);
        self.send(op, &body).await
    }

    /// The next message for the handler, or `None` once the client has
    /// closed the channel or sent `DISCONNECTING`.
    ///
    /// `PONG`, `ACK_SYNC`, `ACK` and `DISCONNECTING` are handled here and
    /// never returned. Everything else is, including opcodes andris does
    /// not know; pass those to [`ChannelConn::ignore`].
    ///
    /// Cancel-safe: a message is either returned or left buffered.
    pub async fn recv(&mut self) -> Result<Option<ReceivedMessage>> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(Some(message));
        }
        loop {
            match self.next_event().await? {
                Event::Channel(message) => return Ok(Some(message)),
                Event::Common => {}
                Event::Closed => return Ok(None),
            }
        }
    }

    /// Close the channel in an orderly way: end andris's side of the
    /// stream, then read and discard whatever the client sends until it
    /// closes its side. Dropping a socket with unread data in it resets
    /// the connection, and a reset can discard what andris last sent
    /// before the client has read it. Not cancel-safe, and it waits as
    /// long as the client keeps the channel open, so the caller bounds it.
    pub async fn close(&mut self) -> Result<()> {
        self.stream
            .shutdown()
            .await
            .with_context(|| format!("closing the {} channel", O::CHANNEL.name()))?;
        while self.recv().await?.is_some() {}
        Ok(())
    }

    /// Log, once per opcode, that a message was read and ignored.
    pub fn ignore(&mut self, message: &ReceivedMessage) {
        let code = message.header.message_type;
        if self.ignored.insert(code) {
            debug!(
                session = self.session.id(),
                channel = O::CHANNEL.name(),
                opcode = code,
                name = O::ryll_client_name(code),
                size = message.header.message_size,
                "ignoring a client message (logged once per opcode)"
            );
        }
    }

    /// Whether the client has sent anything this connection has read but
    /// [`ChannelConn::recv`] has not yet returned or handled: a message,
    /// or part of one. False once andris has caught up with everything
    /// read from the socket, though more may be waiting in it.
    pub fn has_unread(&self) -> bool {
        !self.pending.is_empty() || !self.buf.is_empty()
    }

    /// The session this channel belongs to.
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// Read the next message, handling it here if it is a common one.
    ///
    /// Cancel-safe: there is no await between taking a message off the
    /// buffer and returning it.
    async fn next_event(&mut self) -> Result<Event> {
        if self.closed {
            return Ok(Event::Closed);
        }
        loop {
            if let Some(message) = self
                .buf
                .take_message(O::MAX_CLIENT_BODY)
                .with_context(|| format!("reading the {} channel", O::CHANNEL.name()))?
            {
                return Ok(match self.handle_common(&message)? {
                    Common::Handled => Event::Common,
                    Common::Close => {
                        self.closed = true;
                        Event::Closed
                    }
                    Common::Channel => Event::Channel(message),
                });
            }
            let mut chunk = [0u8; READ_CHUNK];
            let read = self.stream.read(&mut chunk).await?;
            self.buf.extend_from_slice(&chunk[..read]);
            if read == 0 {
                if !self.buf.is_empty() {
                    debug!(
                        channel = O::CHANNEL.name(),
                        bytes = self.buf.len(),
                        "the client closed the channel mid-message"
                    );
                }
                self.closed = true;
                return Ok(Event::Closed);
            }
        }
    }

    /// Handle the messages every channel shares.
    fn handle_common(&mut self, message: &ReceivedMessage) -> Result<Common> {
        let channel = O::CHANNEL.name();
        match message.header.message_type {
            main_client::PONG => {
                let pong = Pong::decode(message.payload()).context("a malformed PONG")?;
                match round_trip(self.session.clock_ns(), pong.timestamp) {
                    Some(rtt) => {
                        self.session.counters().ping_round_trip(rtt);
                        debug!(
                            channel,
                            id = pong.id,
                            rtt_us = rtt.as_micros(),
                            "PONG round trip"
                        );
                    }
                    None => debug!(
                        channel,
                        id = pong.id,
                        timestamp = pong.timestamp,
                        "a PONG with an implausible timestamp; not recorded"
                    ),
                }
                Ok(Common::Handled)
            }
            main_client::ACK_SYNC => {
                let sync = AckSync::decode(message.payload()).context("a malformed ACK_SYNC")?;
                match &mut self.ack {
                    Some(ack) => ack.ack_sync(sync.generation),
                    None => debug!(channel, "ACK_SYNC on a channel without acks; ignored"),
                }
                Ok(Common::Handled)
            }
            main_client::ACK => {
                if let Some(lag) = self.ack.as_mut().and_then(AckWindow::ack) {
                    self.session.counters().ack_lag(lag);
                }
                Ok(Common::Handled)
            }
            main_client::DISCONNECTING => {
                match Disconnecting::decode(message.payload()) {
                    Ok(disconnecting) => debug!(
                        channel,
                        reason = ?disconnecting.reason_kind(),
                        "the client is disconnecting"
                    ),
                    Err(_) => debug!(channel, "the client is disconnecting"),
                }
                Ok(Common::Close)
            }
            _ => Ok(Common::Channel),
        }
    }
}

impl<O: AckedOpcode> ChannelConn<O> {
    /// Send `SET_ACK` with `window`, starting a new ack generation. From
    /// here every message sent counts against the window.
    pub async fn start_acks(&mut self, window: u32) -> Result<()> {
        let generation = self
            .ack
            .as_ref()
            .map_or(1, |ack| ack.generation.wrapping_add(1));
        self.ack = Some(AckWindow::new(generation, window));
        self.send_msg(O::SET_ACK, &SetAck { generation, window })
            .await
    }

    /// False while more than twice the ack window is unacknowledged.
    /// Always true before [`ChannelConn::start_acks`].
    pub fn may_send(&self) -> bool {
        self.ack.as_ref().is_none_or(AckWindow::may_send)
    }

    /// The messages sent and not yet acknowledged.
    pub fn unacked(&self) -> u64 {
        self.ack.as_ref().map_or(0, |ack| ack.unacked)
    }

    /// Read from the client until [`ChannelConn::may_send`] is true.
    /// Channel messages that arrive meanwhile are kept, up to
    /// [`MAX_PENDING`], for [`ChannelConn::recv`] to return in order.
    ///
    /// Cancel-safe: a message read is kept, with no await in between, so
    /// a dropped call loses nothing.
    pub async fn wait_for_room(&mut self) -> Result<()> {
        while !self.may_send() {
            match self.next_event().await? {
                Event::Channel(message) => {
                    if self.pending.len() >= MAX_PENDING {
                        bail!(
                            "the client sent more than {MAX_PENDING} {} messages while \
                             withholding ACKs",
                            O::CHANNEL.name()
                        );
                    }
                    self.pending.push_back(message);
                }
                Event::Common => {}
                Event::Closed => bail!(
                    "the client closed the {} channel while andris waited for ACKs",
                    O::CHANNEL.name()
                ),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::opcodes::{CursorOpcode, DisplayOpcode, InputsOpcode};
    use crate::server::session::SessionRegistry;
    use shakenfist_spice_protocol::display_client;
    use shakenfist_spice_protocol::messages::MessageHeader;
    use tokio::io::DuplexStream;

    const WAIT: Duration = Duration::from_secs(5);

    fn session() -> Arc<Session> {
        SessionRegistry::new()
            .begin("192.0.2.1:5900".parse().unwrap())
            .unwrap()
    }

    fn pair<O: ServerOpcode>() -> (ChannelConn<O>, DuplexStream) {
        let (server, client) = tokio::io::duplex(1 << 20);
        (ChannelConn::new(server, session()), client)
    }

    async fn client_send(client: &mut DuplexStream, opcode: u16, body: &[u8]) {
        client.write_all(&make_message(opcode, body)).await.unwrap();
    }

    async fn client_send_msg<T: WireType>(client: &mut DuplexStream, opcode: u16, message: &T) {
        let mut body = Vec::new();
        message.write(&mut body);
        client_send(client, opcode, &body).await;
    }

    /// Read one framed message the server sent.
    async fn client_recv(client: &mut DuplexStream) -> (u16, Vec<u8>) {
        let mut header = [0u8; MessageHeader::SIZE];
        client.read_exact(&mut header).await.unwrap();
        let header = MessageHeader::read(&header).unwrap();
        let mut body = vec![0; header.message_size as usize];
        client.read_exact(&mut body).await.unwrap();
        (header.message_type, body)
    }

    #[test]
    fn implausible_round_trips_are_discarded() {
        let second = 1_000_000_000;
        assert_eq!(
            round_trip(5 * second, 4 * second),
            Some(Duration::from_secs(1))
        );
        assert_eq!(round_trip(5, 5), Some(Duration::ZERO));
        assert_eq!(round_trip(4 * second, 5 * second), None, "the future");
        assert_eq!(
            round_trip(61 * second, second),
            Some(MAX_PLAUSIBLE_RTT),
            "exactly the limit"
        );
        assert_eq!(round_trip(62 * second, second), None, "over a minute");
        assert_eq!(round_trip(u64::MAX, 0), None);
    }

    #[tokio::test]
    async fn only_plausible_pongs_are_recorded() {
        let (mut conn, mut client) = pair::<DisplayOpcode>();
        for (id, timestamp) in [(1, u64::MAX), (2, 0)] {
            client_send_msg(&mut client, main_client::PONG, &Pong { id, timestamp }).await;
        }
        client_send(&mut client, display_client::INIT, &[0; 14]).await;
        let message = tokio::time::timeout(WAIT, conn.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(message.header.message_type, display_client::INIT);
        // Only the second: the session began moments ago.
        assert_eq!(conn.session().stats().ping.count, 1);
    }

    #[test]
    fn ack_lag_is_taken_from_the_message_completing_each_window() {
        let mut ack = AckWindow::new(1, 2);
        ack.ack_sync(1);
        assert_eq!(ack.ack(), None, "nothing is outstanding");
        ack.sent();
        assert!(ack.window_ends.is_empty(), "a window is not yet complete");
        ack.sent();
        ack.sent();
        ack.sent();
        assert_eq!(ack.window_ends.len(), 2);
        assert!(ack.ack().is_some());
        assert!(ack.ack().is_some());
        assert_eq!(ack.ack(), None);

        // Before ACK_SYNC an ACK counts for nothing, lag included.
        let mut unsynced = AckWindow::new(1, 1);
        unsynced.sent();
        assert_eq!(unsynced.ack(), None);
        assert_eq!(unsynced.window_ends.len(), 1);
    }

    #[test]
    fn ack_window_accounting() {
        let mut ack = AckWindow::new(1, 2);
        for _ in 0..4 {
            ack.sent();
        }
        assert!(ack.may_send(), "four unacked is exactly twice the window");
        ack.sent();
        assert!(!ack.may_send());
        ack.ack();
        assert!(!ack.may_send(), "an ACK before ACK_SYNC does not count");
        ack.ack_sync(2);
        ack.ack();
        assert!(
            !ack.may_send(),
            "an ACK_SYNC for another generation does not count"
        );
        ack.ack_sync(1);
        ack.ack();
        assert!(ack.may_send());
        assert_eq!(ack.unacked, 3);
        for _ in 0..3 {
            ack.ack();
        }
        assert_eq!(ack.unacked, 0, "acks saturate at zero");
    }

    #[test]
    fn client_caps_bits() {
        let caps = ClientCaps {
            common: vec![0b1011],
            channel: vec![1 << 5, 1 << 1],
        };
        assert!(caps.has_common_cap(0));
        assert!(!caps.has_common_cap(2));
        assert!(caps.has_common_cap(3));
        assert!(!caps.has_common_cap(64));
        assert!(caps.has_channel_cap(5));
        assert!(caps.has_channel_cap(33));
        assert!(!caps.has_channel_cap(32));
        assert!(caps.has_channel_mask(1 << 5));
        assert!(!caps.has_channel_mask((1 << 5) | 1));
    }

    #[tokio::test]
    async fn send_frames_with_the_opcode() {
        let (mut conn, mut client) = pair::<InputsOpcode>();
        conn.send(InputsOpcode::MouseMotionAck, &[]).await.unwrap();
        conn.send(InputsOpcode::Init, &[1, 2, 3, 4]).await.unwrap();
        assert_eq!(
            client_recv(&mut client).await,
            (InputsOpcode::MouseMotionAck.code(), vec![])
        );
        assert_eq!(
            client_recv(&mut client).await,
            (InputsOpcode::Init.code(), vec![1, 2, 3, 4])
        );
    }

    #[tokio::test]
    async fn recv_handles_common_messages_and_returns_the_rest() {
        let (mut conn, mut client) = pair::<DisplayOpcode>();
        client_send_msg(
            &mut client,
            main_client::PONG,
            &Pong {
                id: 1,
                timestamp: 0,
            },
        )
        .await;
        client_send(&mut client, main_client::ACK, &[]).await;
        client_send_msg(
            &mut client,
            main_client::ACK_SYNC,
            &AckSync { generation: 1 },
        )
        .await;
        client_send(&mut client, display_client::INIT, &[9; 14]).await;
        client_send(&mut client, 999, &[]).await;

        let message = tokio::time::timeout(WAIT, conn.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(message.header.message_type, display_client::INIT);
        assert_eq!(message.payload(), &[9; 14]);
        let message = tokio::time::timeout(WAIT, conn.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(message.header.message_type, 999);
        conn.ignore(&message);

        drop(client);
        assert!(conn.recv().await.unwrap().is_none(), "EOF closes");
    }

    #[tokio::test]
    async fn disconnecting_closes_the_channel() {
        let (mut conn, mut client) = pair::<InputsOpcode>();
        client_send_msg(
            &mut client,
            main_client::DISCONNECTING,
            &Disconnecting {
                timestamp: 0,
                reason: 0,
            },
        )
        .await;
        client_send(&mut client, 101, &[]).await;
        assert!(conn.recv().await.unwrap().is_none());
        assert!(conn.recv().await.unwrap().is_none(), "stays closed");
    }

    #[tokio::test]
    async fn an_oversized_message_is_an_error() {
        let (mut conn, mut client) = pair::<CursorOpcode>();
        // Only the header: the limit is checked before the body arrives.
        let mut header = Vec::new();
        MessageHeader {
            message_type: 101,
            message_size: 4 * 1024 + 1,
        }
        .write(&mut header)
        .unwrap();
        client.write_all(&header).await.unwrap();
        assert!(conn.recv().await.is_err());
    }

    #[tokio::test]
    async fn recv_is_cancel_safe() {
        let (mut conn, mut client) = pair::<InputsOpcode>();
        // Half a message, then a cancelled recv, then the rest.
        let message = make_message(101, &[1, 2, 3]);
        client.write_all(&message[..4]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), conn.recv())
                .await
                .is_err()
        );
        client.write_all(&message[4..]).await.unwrap();
        let got = tokio::time::timeout(WAIT, conn.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(got.payload(), &[1, 2, 3]);
    }

    #[tokio::test]
    async fn wait_for_room_waits_for_acks_and_keeps_messages() {
        let (mut conn, mut client) = pair::<CursorOpcode>();
        conn.start_acks(2).await.unwrap();
        let (opcode, body) = client_recv(&mut client).await;
        assert_eq!(opcode, CursorOpcode::SetAck.code());
        assert_eq!(
            SetAck::decode(&body).unwrap(),
            SetAck {
                generation: 1,
                window: 2
            }
        );
        // SET_ACK counts: four more makes five unacked, over twice two.
        for _ in 0..4 {
            conn.send(CursorOpcode::Set, &[]).await.unwrap();
        }
        assert_eq!(conn.unacked(), 5);
        assert!(!conn.may_send());

        // A channel message, then the ACK_SYNC and an ACK, arrive while
        // the server waits.
        let waiter = tokio::spawn(async move {
            conn.wait_for_room().await.unwrap();
            conn
        });
        client_send(&mut client, 101, &[7]).await;
        client_send_msg(
            &mut client,
            main_client::ACK_SYNC,
            &AckSync { generation: 1 },
        )
        .await;
        client_send(&mut client, main_client::ACK, &[]).await;
        let mut conn = tokio::time::timeout(WAIT, waiter).await.unwrap().unwrap();
        assert!(conn.may_send());
        assert_eq!(conn.unacked(), 3);
        let kept = conn.recv().await.unwrap().unwrap();
        assert_eq!(kept.payload(), &[7], "the message that arrived is kept");
    }

    #[tokio::test]
    async fn wait_for_room_caps_what_it_keeps() {
        let (mut conn, mut client) = pair::<CursorOpcode>();
        conn.start_acks(0).await.unwrap();
        assert!(!conn.may_send(), "SET_ACK alone exceeds a zero window");
        for _ in 0..=MAX_PENDING {
            client_send(&mut client, 101, &[]).await;
        }
        let result = tokio::time::timeout(WAIT, conn.wait_for_room())
            .await
            .unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn wait_for_room_fails_if_the_client_leaves() {
        let (mut conn, client) = pair::<DisplayOpcode>();
        conn.start_acks(0).await.unwrap();
        drop(client);
        assert!(conn.wait_for_room().await.is_err());
    }
}
