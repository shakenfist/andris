//! Sessions: one client's set of channels, and the registry that holds
//! the one session andris serves at a time.
//!
//! A main-channel link that authenticates begins a new session and
//! cancels the one before it. Every channel task of a session races its
//! handler against [`Session::cancelled`], so cancelling a session drops
//! each of its channels' sockets without a message, as spice-server
//! does when a new client replaces an old one.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use shakenfist_spice_protocol::{MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER};
use tokio::sync::watch;
use tracing::info;

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

    /// The mode a `SPICE_MOUSE_MODE_*` value names, if any.
    pub fn from_wire(value: u32) -> Option<Self> {
        match value {
            MOUSE_MODE_SERVER => Some(MouseMode::Server),
            MOUSE_MODE_CLIENT => Some(MouseMode::Client),
            _ => None,
        }
    }
}

/// One client's session: the channels linked with its id.
#[derive(Debug)]
pub struct Session {
    id: u32,
    started: Instant,
    mouse_mode: watch::Sender<MouseMode>,
    cancel: Cancel,
}

impl Session {
    fn new(id: u32) -> Self {
        Self {
            id,
            started: Instant::now(),
            mouse_mode: watch::Sender::new(MouseMode::Client),
            cancel: Cancel::new(),
        }
    }

    /// The session id: the main channel's `INIT` carries it, and each
    /// secondary channel's link must name it.
    pub fn id(&self) -> u32 {
        self.id
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
}

#[derive(Debug, Default)]
struct RegistryState {
    current: Option<Arc<Session>>,
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

    /// Begin a new session with a random non-zero id, cancelling the
    /// current one. Fails once the registry is closed.
    pub fn begin(&self) -> Result<Arc<Session>> {
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
        let session = Arc::new(Session::new(id));
        if let Some(old) = state.current.replace(session.clone()) {
            info!(
                old = old.id(),
                new = id,
                "a new client replaces the current session"
            );
            old.cancel();
        } else {
            info!(session = id, "session begins");
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

    /// Cancel the current session and refuse new ones.
    pub fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        if let Some(session) = state.current.take() {
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

    #[test]
    fn begin_replaces_and_cancels_the_current_session() {
        let registry = SessionRegistry::new();
        let first = registry.begin().unwrap();
        assert_ne!(first.id(), 0);
        assert_eq!(registry.current_id(), Some(first.id()));
        assert!(registry.lookup(first.id()).is_some());

        let second = registry.begin().unwrap();
        assert_ne!(second.id(), first.id());
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(registry.lookup(first.id()).is_none());
        assert!(registry.lookup(second.id()).is_some());
    }

    #[test]
    fn ending_an_old_session_leaves_the_current_one() {
        let registry = SessionRegistry::new();
        let first = registry.begin().unwrap();
        let second = registry.begin().unwrap();
        registry.end(&first);
        assert_eq!(registry.current_id(), Some(second.id()));
        registry.end(&second);
        assert_eq!(registry.current_id(), None);
        assert!(second.is_cancelled());
    }

    #[test]
    fn close_cancels_and_refuses() {
        let registry = SessionRegistry::new();
        let session = registry.begin().unwrap();
        registry.close();
        assert!(session.is_cancelled());
        assert!(registry.begin().is_err());
        assert_eq!(registry.current_id(), None);
    }

    #[test]
    fn mouse_mode_wire_values() {
        for mode in [MouseMode::Server, MouseMode::Client] {
            assert_eq!(MouseMode::from_wire(mode.to_wire()), Some(mode));
        }
        assert_eq!(MouseMode::from_wire(0), None);
        let session = Session::new(1);
        assert_eq!(*session.mouse_mode().borrow(), MouseMode::Client);
    }
}
