//! Sessions: one client's set of channels, and the registry that holds
//! the one session andris serves at a time.
//!
//! A main-channel link that authenticates begins a new session and
//! replaces the one before it. Every channel task of a session races its
//! handler against [`Session::cancelled`], so cancelling a session drops
//! each of its channels' sockets without a message. A replaced session is
//! not cancelled straight away, though: spice-server drops the old client
//! without a word, and an auto-reconnecting client takes that for a
//! network fault and takes the session straight back. So
//! [`SessionRegistry::begin`] only marks the old session replaced, its
//! main channel tells the client why (see [`Session::replaced`]), and the
//! session ends when that channel does. Its other channels close as soon
//! as the goodbye is written, and its inputs reach the desktop no more
//! from the moment it is replaced.
//!
//! A session holds at most one channel of each type. A new link of a
//! type the session already has replaces the old channel: claiming the
//! type with [`Session::claim_channel`] cancels the previous claim, whose
//! task then drops its handler and socket, running its `Drop` guards.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use shakenfist_spice_protocol::{MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER};
use tokio::sync::watch;
use tracing::info;

use super::link::ServedChannel;
use super::stats::{Stats, StatsSnapshot};

/// A one-way cancel signal that any number of tasks can await.
///
/// This is a `watch` channel holding `true` once cancelled: the sender
/// lives as long as the signal, so a waiter never sees it dropped.
#[derive(Debug)]
pub struct Cancel {
    tx: watch::Sender<bool>,
}

impl Cancel {
    pub fn new() -> Self {
        Self {
            tx: watch::Sender::new(false),
        }
    }

    /// Cancel. Every current and future [`Cancel::cancelled`] returns.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Return once [`Cancel::cancel`] has been called. Cancel-safe.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        // `self` holds the sender, so this cannot fail; if it somehow
        // did, treating that as cancelled is the safe answer.
        let _ = rx.wait_for(|cancelled| *cancelled).await;
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

/// How the client reports the pointer: by absolute position (`Client`),
/// or by relative motion (`Server`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    Server,
    Client,
}

impl MouseMode {
    /// The mode's `SPICE_MOUSE_MODE_*` value.
    pub fn to_wire(self) -> u32 {
        match self {
            MouseMode::Server => MOUSE_MODE_SERVER,
            MouseMode::Client => MOUSE_MODE_CLIENT,
        }
    }

    /// The mode's `SPICE_MOUSE_MODE_*` value as the `u16` that
    /// `MAIN_MOUSE_MODE` carries.
    pub fn to_wire_u16(self) -> u16 {
        // Both values are checked to fit when this compiles.
        const SERVER: u16 = narrow(MOUSE_MODE_SERVER);
        const CLIENT: u16 = narrow(MOUSE_MODE_CLIENT);
        match self {
            MouseMode::Server => SERVER,
            MouseMode::Client => CLIENT,
        }
    }

    /// The mode a `SPICE_MOUSE_MODE_*` value names, if any.
    pub fn from_wire(value: u32) -> Option<Self> {
        match value {
            MOUSE_MODE_SERVER => Some(MouseMode::Server),
            MOUSE_MODE_CLIENT => Some(MouseMode::Client),
            _ => None,
        }
    }
}

/// `value` as a `u16`. Called only in constants, where a value that does
/// not fit fails the build.
const fn narrow(value: u32) -> u16 {
    assert!(
        value <= u16::MAX as u32,
        "a mouse mode does not fit in 16 bits"
    );
    value as u16
}

/// One client's session: the channels linked with its id.
#[derive(Debug)]
pub struct Session {
    id: u32,
    /// The address the session's main channel linked from.
    peer: SocketAddr,
    started: Instant,
    mouse_mode: watch::Sender<MouseMode>,
    cancel: Cancel,
    /// Fired once a new client replaces this session.
    replaced: Cancel,
    /// Fired to close every channel but main.
    secondary_closed: Cancel,
    /// The address of the client that replaced this session.
    replaced_by: OnceLock<SocketAddr>,
    /// The channel serving each type, by its claim's cancel signal.
    channels: Mutex<HashMap<ServedChannel, Arc<Cancel>>>,
    stats: Stats,
}

impl Session {
    fn new(id: u32, peer: SocketAddr) -> Self {
        Self {
            id,
            peer,
            started: Instant::now(),
            mouse_mode: watch::Sender::new(MouseMode::Client),
            cancel: Cancel::new(),
            replaced: Cancel::new(),
            secondary_closed: Cancel::new(),
            replaced_by: OnceLock::new(),
            channels: Mutex::new(HashMap::new()),
            stats: Stats::default(),
        }
    }

    fn channels(&self) -> MutexGuard<'_, HashMap<ServedChannel, Arc<Cancel>>> {
        // Each update is a single insert or remove, so a panic while
        // holding the lock cannot leave the map half-updated.
        self.channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claim `channel` for a newly linked channel, cancelling the claim of
    /// the channel of that type already serving, if any. The returned
    /// claim's [`ChannelClaim::replaced`] returns once a later link claims
    /// the type in turn.
    pub fn claim_channel(self: &Arc<Self>, channel: ServedChannel) -> ChannelClaim {
        let cancel = Arc::new(Cancel::new());
        if let Some(old) = self.channels().insert(channel, cancel.clone()) {
            info!(
                session = self.id,
                channel = channel.name(),
                "a new link replaces the session's channel of this type"
            );
            old.cancel();
        }
        ChannelClaim {
            session: self.clone(),
            channel,
            cancel,
        }
    }

    /// The session id: the main channel's `INIT` carries it, and each
    /// secondary channel's link must name it.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The address the session's main channel linked from, for the log.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The session's metrics, for its channels to update.
    pub fn counters(&self) -> &Stats {
        &self.stats
    }

    /// The session's metrics so far.
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// When the session began: the zero of its clock, from which the
    /// multimedia time is counted.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// Time since the session began.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The session's monotonic clock, in nanoseconds since it began.
    /// `PING` timestamps use it, so a `PONG` carries its own send time.
    pub fn clock_ns(&self) -> u64 {
        u64::try_from(self.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// The session's mouse mode. The main channel changes it and the
    /// inputs channel reads it; `subscribe()` gives a receiver.
    pub fn mouse_mode(&self) -> &watch::Sender<MouseMode> {
        &self.mouse_mode
    }

    /// End the session: each of its channels closes.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Return once the session has been cancelled. Cancel-safe.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await
    }

    /// Mark the session replaced by a client from `by`. Only the first
    /// replacement is recorded; there can be no other.
    fn replace(&self, by: SocketAddr) {
        let _ = self.replaced_by.set(by);
        self.replaced.cancel();
    }

    /// Whether a new client has replaced this session. Its inputs
    /// channel checks this, so the replaced client's input does not reach
    /// the desktop the new client now has.
    pub fn is_replaced(&self) -> bool {
        self.replaced.is_cancelled()
    }

    /// The address of the client that replaced this session, if one has.
    pub fn replaced_by(&self) -> Option<SocketAddr> {
        self.replaced_by.get().copied()
    }

    /// Return once a new client has replaced this session. Cancel-safe.
    ///
    /// The session carries on until its main channel has told the client
    /// why it is going (the main channel's farewell) and closed, and then
    /// ends as any session does when its main channel does. The server
    /// ends it regardless if that takes too long.
    pub async fn replaced(&self) {
        self.replaced.cancelled().await
    }

    /// Close every channel of the session but main, which carries on
    /// alone. A replaced session's main channel calls this once its
    /// goodbye is written: a client that saw another channel close first
    /// could take that for a fault and reconnect.
    pub fn close_secondary_channels(&self) {
        self.secondary_closed.cancel();
    }

    /// Return once [`Session::close_secondary_channels`] has been called.
    /// Cancel-safe.
    pub async fn secondary_channels_closed(&self) {
        self.secondary_closed.cancelled().await
    }
}

/// A channel's hold on its type within a session. Dropping it frees the
/// type, unless a later claim has already taken it.
#[derive(Debug)]
pub struct ChannelClaim {
    session: Arc<Session>,
    channel: ServedChannel,
    cancel: Arc<Cancel>,
}

impl ChannelClaim {
    /// Return once a later link has claimed this channel's type.
    /// Cancel-safe.
    pub async fn replaced(&self) {
        self.cancel.cancelled().await
    }

    pub fn is_replaced(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

impl Drop for ChannelClaim {
    fn drop(&mut self) {
        let mut channels = self.session.channels();
        if channels
            .get(&self.channel)
            .is_some_and(|current| Arc::ptr_eq(current, &self.cancel))
        {
            channels.remove(&self.channel);
        }
    }
}

#[derive(Debug, Default)]
struct RegistryState {
    current: Option<Arc<Session>>,
    /// Sessions replaced and not yet ended: each lives on while its main
    /// channel says goodbye, and a shutdown must end those too.
    replaced: Vec<Weak<Session>>,
    closed: bool,
}

/// Holds the one session andris serves at a time.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    state: Mutex<RegistryState>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, RegistryState> {
        // The state is replaced whole under the lock, so a panic while
        // holding it cannot leave it half-updated: recover from poison.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Begin a new session for a client from `peer`, with a random
    /// non-zero id, replacing the current one (see
    /// [`Session::replaced`]). Fails once the registry is closed.
    pub fn begin(&self, peer: SocketAddr) -> Result<Arc<Session>> {
        let mut state = self.state();
        if state.closed {
            bail!("andris is shutting down, so no new session can begin");
        }
        let previous = state.current.as_ref().map(|s| s.id());
        let id = loop {
            let id: u32 = rand::random();
            if id != 0 && Some(id) != previous {
                break id;
            }
        };
        let session = Arc::new(Session::new(id, peer));
        if let Some(old) = state.current.replace(session.clone()) {
            info!(
                old = old.id(),
                old_peer = %old.peer(),
                new = id,
                new_peer = %peer,
                "a new client replaces the current session"
            );
            old.replace(peer);
            state.replaced.retain(|s| s.strong_count() > 0);
            state.replaced.push(Arc::downgrade(&old));
        } else {
            info!(session = id, %peer, "session begins");
        }
        Ok(session)
    }

    /// The current session, if its id is `id`.
    pub fn lookup(&self, id: u32) -> Option<Arc<Session>> {
        self.state()
            .current
            .as_ref()
            .filter(|session| session.id() == id && !session.is_cancelled())
            .cloned()
    }

    /// The current session's id, if there is one.
    pub fn current_id(&self) -> Option<u32> {
        self.state().current.as_ref().map(|session| session.id())
    }

    /// Cancel `session`, and forget it if it is still the current one.
    pub fn end(&self, session: &Session) {
        session.cancel();
        let mut state = self.state();
        if state
            .current
            .as_ref()
            .is_some_and(|s| s.id() == session.id())
        {
            info!(session = session.id(), "session ends");
            state.current = None;
        }
    }

    /// Cancel the current session, and any replaced one still saying
    /// goodbye, and refuse new ones.
    pub fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        if let Some(session) = state.current.take() {
            session.cancel();
        }
        for session in state.replaced.drain(..).filter_map(|s| s.upgrade()) {
            session.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_wakes_waiters_before_and_after() {
        let cancel = Arc::new(Cancel::new());
        let waiter = {
            let cancel = cancel.clone();
            tokio::spawn(async move { cancel.cancelled().await })
        };
        assert!(!cancel.is_cancelled());
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
        // A waiter that arrives late returns at once.
        tokio::time::timeout(Duration::from_secs(5), cancel.cancelled())
            .await
            .unwrap();
    }

    fn peer(n: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, n], 5900))
    }

    /// A new session takes the old one's place at once, but the old one
    /// is only marked replaced: its main channel ends it once it has told
    /// the client why.
    #[test]
    fn begin_replaces_the_current_session() {
        let registry = SessionRegistry::new();
        let first = registry.begin(peer(1)).unwrap();
        assert_ne!(first.id(), 0);
        assert_eq!(first.peer(), peer(1));
        assert_eq!(registry.current_id(), Some(first.id()));
        assert!(registry.lookup(first.id()).is_some());
        assert_eq!(first.replaced_by(), None);

        let second = registry.begin(peer(2)).unwrap();
        assert_ne!(second.id(), first.id());
        assert_eq!(first.replaced_by(), Some(peer(2)));
        assert!(first.is_replaced());
        assert!(!first.is_cancelled());
        assert_eq!(second.replaced_by(), None);
        assert!(!second.is_replaced());
        assert!(!second.is_cancelled());
        assert!(registry.lookup(first.id()).is_none());
        assert!(registry.lookup(second.id()).is_some());

        registry.end(&first);
        assert!(first.is_cancelled());
        assert_eq!(registry.current_id(), Some(second.id()));
    }

    #[tokio::test]
    async fn replaced_returns_once_replaced() {
        let registry = SessionRegistry::new();
        let first = registry.begin(peer(1)).unwrap();
        let waiter = {
            let first = first.clone();
            tokio::spawn(async move { first.replaced().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        registry.begin(peer(2)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn ending_an_old_session_leaves_the_current_one() {
        let registry = SessionRegistry::new();
        let first = registry.begin(peer(1)).unwrap();
        let second = registry.begin(peer(2)).unwrap();
        registry.end(&first);
        assert_eq!(registry.current_id(), Some(second.id()));
        registry.end(&second);
        assert_eq!(registry.current_id(), None);
        assert!(second.is_cancelled());
    }

    #[test]
    fn close_cancels_and_refuses() {
        let registry = SessionRegistry::new();
        let session = registry.begin(peer(1)).unwrap();
        registry.close();
        assert!(session.is_cancelled());
        assert!(registry.begin(peer(1)).is_err());
        assert_eq!(registry.current_id(), None);
    }

    /// A replaced session lives on while it says goodbye, out of the
    /// registry's sight; a shutdown must still end it at once.
    #[test]
    fn close_cancels_replaced_sessions_too() {
        let registry = SessionRegistry::new();
        let first = registry.begin(peer(1)).unwrap();
        let second = registry.begin(peer(2)).unwrap();
        let third = registry.begin(peer(3)).unwrap();
        assert!(!first.is_cancelled() && !second.is_cancelled());
        registry.close();
        assert!(first.is_cancelled());
        assert!(second.is_cancelled());
        assert!(third.is_cancelled());
    }

    /// The registry does not keep sessions that have ended.
    #[test]
    fn ended_replaced_sessions_are_forgotten() {
        let registry = SessionRegistry::new();
        let first = registry.begin(peer(1)).unwrap();
        registry.begin(peer(2)).unwrap();
        registry.end(&first);
        drop(first);
        registry.begin(peer(3)).unwrap();
        assert_eq!(registry.state().replaced.len(), 1);
    }

    #[test]
    fn a_second_claim_replaces_the_first() {
        let session = SessionRegistry::new().begin(peer(1)).unwrap();
        let first = session.claim_channel(ServedChannel::Display);
        let inputs = session.claim_channel(ServedChannel::Inputs);
        assert!(!first.is_replaced());

        let second = session.claim_channel(ServedChannel::Display);
        assert!(first.is_replaced());
        assert!(!second.is_replaced());
        assert!(!inputs.is_replaced(), "other types are untouched");

        // The replaced claim's drop leaves the new one in place.
        drop(first);
        let third = session.claim_channel(ServedChannel::Display);
        assert!(second.is_replaced());

        // Once dropped, a type is free and a new claim replaces nothing.
        drop(third);
        drop(second);
        assert!(session.channels().get(&ServedChannel::Display).is_none());
        let fourth = session.claim_channel(ServedChannel::Display);
        assert!(!fourth.is_replaced());
    }

    #[test]
    fn mouse_mode_wire_values() {
        for mode in [MouseMode::Server, MouseMode::Client] {
            assert_eq!(MouseMode::from_wire(mode.to_wire()), Some(mode));
            assert_eq!(u32::from(mode.to_wire_u16()), mode.to_wire());
        }
        assert_eq!(MouseMode::from_wire(0), None);
        let session = Session::new(1, peer(1));
        assert_eq!(*session.mouse_mode().borrow(), MouseMode::Client);
    }
}
