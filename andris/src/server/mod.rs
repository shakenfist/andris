//! The SPICE server: listeners, link and auth, sessions and channels.
//!
//! - [`link`] links and authenticates one connection;
//! - [`session`] holds the one session andris serves at a time;
//! - [`channel`] frames messages, handles the ones every channel shares,
//!   and keeps the ack window;
//! - [`opcodes`] lists what each channel may send;
//! - [`main_channel`], [`display`], [`inputs`], [`cursor`] and
//!   [`playback`] serve the five channels, and [`agent`] plays the guest
//!   agent on the main channel;
//! - [`updates`] compares captures with what the display channel last
//!   sent, and encodes what changed as `DRAW_COPY`s;
//! - [`keymap`] decodes the inputs channel's set-1 scancodes to evdev
//!   codes;
//! - [`tls`] loads the TLS listener's certificate.
//!
//! # Tasks
//!
//! Each listener runs one accept task. Each accepted connection gets one
//! task, which links and authenticates it and then serves the channel,
//! owning its socket throughout. Every one of these tasks holds a clone of
//! one [`mpsc::Sender`], so [`ServerHandle::stop`] knows they have all
//! ended when its receiver closes.
//!
//! # Limits on unauthenticated peers
//!
//! From accept until its handshake ends, a connection holds a slot for
//! its source address and a permit: one of [`MAX_HANDSHAKES`] shared by
//! the serving listeners, or on a plain listener that only redirects to
//! TLS, one of its own [`MAX_REDIRECTS`]. With none free, a new
//! connection is closed at once, and a flood of the redirect port cannot
//! starve the TLS listener.
//!
//! How many slots one source address may hold is not fixed. While the
//! pool it draws on is quiet it may hold [`MAX_HANDSHAKES_PER_PEER`];
//! as the pool fills, its allowance shrinks to a single slot. See
//! [`peer_allowance`], which explains why a fixed cap bounded nothing:
//! an attacker chooses how many addresses to use, so a cap of four
//! against a pool of eight meant two addresses could hold every permit
//! and keep every other client out, having authenticated nothing.
//!
//! The first stage of a handshake, which a peer can stall having proved
//! nothing, has a shorter time limit than the rest; see
//! [`HandshakeTimeouts`]. At most [`MAX_KEY_GENERATIONS`] RSA keys are
//! generated at once.

pub mod agent;
pub mod channel;
pub mod cursor;
pub mod display;
pub mod inputs;
pub mod keymap;
pub mod link;
pub mod main_channel;
pub mod opcodes;
pub mod playback;
pub mod session;
pub mod stats;
pub mod tls;
pub mod updates;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use shakenfist_spice_protocol::link::{SpiceStream, read_link_mess, send_need_secured};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tracing::{debug, info, warn};

use crate::config::Ticket;
use crate::platform::Platform;
use channel::{ChannelConn, ChannelContext, drive};
pub use display::DisplayOptions;
pub use link::HandshakeTimeouts;
use link::{HandshakeContext, Linked, ServedChannel, timed};
use opcodes::{CursorOpcode, DisplayOpcode, InputsOpcode, MainOpcode, PlaybackOpcode};
use session::{Cancel, Session, SessionRegistry};

/// How many connections may be in their handshake at once, across the
/// listeners that serve SPICE. A handshake is short-lived and costs a
/// socket and a task, so this is set well above
/// [`MAX_HANDSHAKES_PER_PEER`] rather than at the smallest workable
/// number: [`peer_allowance`] can only make slots expensive if one
/// address's share of the pool is small.
///
/// Every handshake that reaches the link reply waits for one of
/// [`MAX_KEY_GENERATIONS`] to make its ticket key, so a full pool queues
/// there. Measured in the devcontainer on an idle machine, a 1024-bit
/// key takes about 22 ms (median of 40; 34 ms at the 90th percentile and
/// 49 ms at the slowest), so the last of a full pool waits behind about
/// 32 generations: of the order of a second, and under two at the
/// slowest seen. That is inside the ten seconds
/// [`HandshakeTimeouts::stage`] allows with room for a host several
/// times busier. That room is an estimate: a busy desktop, whose capture
/// and encoding share the cores and the blocking pool the key generations
/// run on, has not been measured, and a prime search has a long tail
/// that forty samples do not reach. It is why the pool could grow without
/// [`MAX_KEY_GENERATIONS`] growing with it: raising that would let a
/// flood spend more of the desktop's CPU at once, which is the thing it
/// is there to bound.
pub const MAX_HANDSHAKES: usize = 64;

/// The most handshakes one source address may have in progress at once,
/// across every listener, which is what it is allowed while the pool is
/// quiet. Four lets a client link its main channel and then display,
/// cursor and inputs in parallel, as spice-gtk does, including through a
/// kerbside on the same host whose every link comes from one address.
/// [`peer_allowance`] lowers it as the pool fills. IPv6 addresses count
/// by their /64, which is what one host usually has to pick from.
pub const MAX_HANDSHAKES_PER_PEER: usize = 4;

/// How many connections a plain listener that only redirects to TLS may
/// be answering at once. Apart from [`MAX_HANDSHAKES`], so that a flood
/// of the redirect port cannot starve the TLS listener, and above
/// [`MAX_HANDSHAKES_PER_PEER`] for the same reason that pool is.
pub const MAX_REDIRECTS: usize = 16;

/// How many RSA ticket keys may be generated at once.
pub const MAX_KEY_GENERATIONS: usize = 2;

/// How long to pause after a failed accept, so that running out of file
/// descriptors does not become a busy loop.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

/// How long `andris` waits, on SIGINT or SIGTERM, for
/// [`ServerHandle::stop`]: for connections to close and the input they
/// release to reach the desktop.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// Held by every accept and connection task; see the module's Tasks
/// section. Nothing is ever sent on it.
type TaskMarker = mpsc::Sender<()>;

/// The TLS listener.
#[derive(Clone, Debug)]
pub struct TlsOptions {
    pub listen: SocketAddr,
    pub config: Arc<ServerConfig>,
}

/// What the server needs to know to start.
#[derive(Clone)]
pub struct ServerOptions {
    /// The plain listener's address. With TLS it only redirects.
    pub listen: SocketAddr,
    pub tls: Option<TlsOptions>,
    pub ticket: Ticket,
    /// How display channels pace their frames, and whether they may send
    /// JPEG.
    pub display: DisplayOptions,
    /// The handshake's time limits; tests shorten them.
    pub timeouts: HandshakeTimeouts,
}

/// Lets an event that a peer can repeat at will be logged at most once
/// per [`LogThrottle::INTERVAL`], with how many happened since the last
/// log.
#[derive(Debug, Default)]
pub struct LogThrottle {
    state: Mutex<(Option<Instant>, u64)>,
}

impl LogThrottle {
    /// The shortest time between two logs.
    pub const INTERVAL: Duration = Duration::from_secs(10);

    /// Count one event. When it should be logged, returns how many events
    /// there have been since the last log, this one included.
    pub fn should_log(&self) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.1 += 1;
        let now = Instant::now();
        if state
            .0
            .is_none_or(|last| now.duration_since(last) >= Self::INTERVAL)
        {
            let count = state.1;
            *state = (Some(now), 0);
            Some(count)
        } else {
            None
        }
    }
}

/// The key a source address's handshakes are counted under: an IPv4
/// address (including one mapped into IPv6) as it is, and an IPv6
/// address by its /64.
fn peer_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            let prefix = u128::from(v6) & !((1u128 << 64) - 1);
            IpAddr::V6(Ipv6Addr::from(prefix))
        }
    }
}

/// How many handshakes one source address may have in progress, given
/// that `free` of a pool's `limit` permits are unused.
///
/// While at least half the pool is free -- the ordinary case, since a
/// handshake lasts milliseconds -- this is [`MAX_HANDSHAKES_PER_PEER`],
/// so a client linking its channels in parallel is unaffected. Past that
/// it falls with the free permits and reaches one when the pool is nearly
/// full.
///
/// The point is what it costs an attacker. A fixed cap per address
/// bounds one address and nothing else, because the attacker decides how
/// many addresses to use; it is the ratio of the cap to the pool that
/// decides how many it needs. Shrinking the allowance as the pool fills
/// means the last permits cost one address each, so holding the pool
/// closed takes tens of addresses instead of two. A flood from many
/// addresses still succeeds, and `docs/running.md` says so.
fn peer_allowance(free: usize, limit: usize) -> usize {
    let limit = limit.max(1);
    if free * 2 >= limit {
        return MAX_HANDSHAKES_PER_PEER;
    }
    (free * 2 * MAX_HANDSHAKES_PER_PEER / limit).clamp(1, MAX_HANDSHAKES_PER_PEER)
}

/// How many handshakes each source address has in progress.
#[derive(Debug, Default)]
struct PeerSlots {
    counts: Mutex<HashMap<IpAddr, usize>>,
}

impl PeerSlots {
    /// Take a slot for `ip`, unless it already holds `allowance` of
    /// them. The allowance comes from [`peer_allowance`] and so varies
    /// with how full the pool is.
    fn try_acquire(self: &Arc<Self>, ip: IpAddr, allowance: usize) -> Option<PeerSlot> {
        let key = peer_key(ip);
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = counts.entry(key).or_insert(0);
        if *count >= allowance {
            return None;
        }
        *count += 1;
        Some(PeerSlot {
            slots: self.clone(),
            key,
        })
    }

    /// How many handshakes `ip` has in progress.
    fn in_progress(&self, ip: IpAddr) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&peer_key(ip))
            .copied()
            .unwrap_or(0)
    }
}

/// One of a source address's handshake slots; dropping it frees it.
#[derive(Debug)]
struct PeerSlot {
    slots: Arc<PeerSlots>,
    key: IpAddr,
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let mut counts = self
            .slots
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = counts.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

/// What a connection holds until its handshake ends.
struct HandshakeSlot {
    _permit: OwnedSemaphorePermit,
    _peer: PeerSlot,
}

/// State every task of the server shares.
struct Shared {
    ticket: Ticket,
    platform: Platform,
    display: DisplayOptions,
    timeouts: HandshakeTimeouts,
    registry: SessionRegistry,
    handshakes: Arc<Semaphore>,
    redirects: Arc<Semaphore>,
    peers: Arc<PeerSlots>,
    key_generations: Arc<Semaphore>,
    /// Throttles for the log lines a peer can provoke at will.
    busy_refusals: LogThrottle,
    peer_refusals: LogThrottle,
    wrong_tickets: LogThrottle,
    stale_sessions: LogThrottle,
    shutdown: Cancel,
}

/// What a listener does with the connections it accepts.
#[derive(Clone)]
enum ListenerKind {
    /// Serve SPICE over plain TCP.
    Plain,
    /// Answer every link with `NeedSecured`, sending the client to the
    /// TLS listener.
    Redirect,
    /// Serve SPICE over TLS.
    Tls(TlsAcceptor),
}

/// A server whose listeners are bound but not yet accepting.
pub struct Server {
    plain: TcpListener,
    tls: Option<(TcpListener, TlsAcceptor)>,
    shared: Arc<Shared>,
}

impl Server {
    /// Bind the listeners `options` names.
    pub async fn bind(options: ServerOptions, platform: Platform) -> Result<Self> {
        let plain = TcpListener::bind(options.listen)
            .await
            .with_context(|| format!("--listen {} cannot be bound", options.listen))?;
        let tls = match options.tls {
            Some(tls) => {
                let listener = TcpListener::bind(tls.listen)
                    .await
                    .with_context(|| format!("--tls-listen {} cannot be bound", tls.listen))?;
                Some((listener, TlsAcceptor::from(tls.config)))
            }
            None => None,
        };
        Ok(Self {
            plain,
            tls,
            shared: Arc::new(Shared {
                ticket: options.ticket,
                platform,
                display: options.display,
                timeouts: options.timeouts,
                registry: SessionRegistry::new(),
                handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
                redirects: Arc::new(Semaphore::new(MAX_REDIRECTS)),
                peers: Arc::new(PeerSlots::default()),
                key_generations: Arc::new(Semaphore::new(MAX_KEY_GENERATIONS)),
                busy_refusals: LogThrottle::default(),
                peer_refusals: LogThrottle::default(),
                wrong_tickets: LogThrottle::default(),
                stale_sessions: LogThrottle::default(),
                shutdown: Cancel::new(),
            }),
        })
    }

    /// Start accepting connections.
    pub fn spawn(self) -> Result<ServerHandle> {
        let (marker, tasks) = mpsc::channel(1);
        let plain_addr = self
            .plain
            .local_addr()
            .context("the plain listener has no address")?;
        let (plain_kind, tls_addr) = match self.tls {
            Some((listener, acceptor)) => {
                let addr = listener
                    .local_addr()
                    .context("the TLS listener has no address")?;
                info!(%addr, "TLS listener ready");
                tokio::spawn(accept_loop(
                    listener,
                    ListenerKind::Tls(acceptor),
                    self.shared.clone(),
                    marker.clone(),
                ));
                (ListenerKind::Redirect, Some(addr))
            }
            None => (ListenerKind::Plain, None),
        };
        if tls_addr.is_some() {
            info!(addr = %plain_addr, "plain listener ready; it redirects clients to TLS");
        } else {
            info!(addr = %plain_addr, "plain listener ready");
        }
        tokio::spawn(accept_loop(
            self.plain,
            plain_kind,
            self.shared.clone(),
            marker,
        ));
        Ok(ServerHandle {
            plain_addr,
            tls_addr,
            shared: self.shared,
            tasks,
        })
    }
}

/// A running server. Dropping it shuts the server down.
pub struct ServerHandle {
    plain_addr: SocketAddr,
    tls_addr: Option<SocketAddr>,
    shared: Arc<Shared>,
    /// Closes once every accept and connection task has ended.
    tasks: mpsc::Receiver<()>,
}

impl ServerHandle {
    /// The plain listener's address.
    pub fn plain_addr(&self) -> SocketAddr {
        self.plain_addr
    }

    /// The TLS listener's address, if there is one.
    pub fn tls_addr(&self) -> Option<SocketAddr> {
        self.tls_addr
    }

    /// The current session's id, if a client is connected.
    pub fn session_id(&self) -> Option<u32> {
        self.shared.registry.current_id()
    }

    /// The current session, if a client is connected. Tests use it to
    /// watch session state such as the mouse mode.
    pub fn session(&self) -> Option<Arc<session::Session>> {
        let id = self.shared.registry.current_id()?;
        self.shared.registry.lookup(id)
    }

    /// How many of the [`MAX_HANDSHAKES`] permits are free. Tests poll
    /// it to see a handshake end.
    pub fn handshakes_available(&self) -> usize {
        self.shared.handshakes.available_permits()
    }

    /// How many handshakes `ip` has in progress.
    pub fn handshakes_from(&self, ip: IpAddr) -> usize {
        self.shared.peers.in_progress(ip)
    }

    /// Stop accepting connections and end the current session. Idempotent.
    pub fn shutdown(&self) {
        self.shared.shutdown.cancel();
        self.shared.registry.close();
    }

    /// Shut down, then wait at most `grace` for every connection to
    /// close and for the input their channels released to reach the
    /// desktop. Returns whether that finished in time.
    ///
    /// A process that exits straight after [`ServerHandle::shutdown`]
    /// can lose those releases, which are only queued, and XTEST does
    /// not release what a departing client holds: a key held at Ctrl-C
    /// would stay held on the desktop.
    pub async fn stop(&mut self, grace: Duration) -> bool {
        self.shutdown();
        let input = self.shared.platform.input.clone();
        let tasks = &mut self.tasks;
        let stopped = async move {
            // Nothing is sent, so this returns once every sender is gone.
            let _ = tasks.recv().await;
            // The lock state is read after every event already queued has
            // been applied, so reading it waits for the releases.
            match tokio::task::spawn_blocking(move || input.lock_state()).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => debug!(
                    error = format!("{e:#}"),
                    "could not confirm the desktop took the released input"
                ),
                Err(e) => debug!(error = %e, "waiting for released input failed"),
            }
        };
        tokio::time::timeout(grace, stopped).await.is_ok()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Take what a new connection from `peer` on a `kind` listener must hold
/// for its handshake, or say why it cannot have it.
///
/// One address's slots are counted across both pools while its allowance
/// is worked out from the pool this listener draws on, so a redirect
/// costs a slot a SPICE handshake could have used. That is deliberate:
/// it errs towards giving an address fewer slots, never more, and a
/// redirect is answered and closed in one write.
///
/// The permit is taken before the allowance is worked out, so that the
/// count the allowance comes from already includes every permit taken
/// by an accept on another listener, rather than one read before them.
/// Its own permit is counted back in as free, which is what the pool
/// had for this connection to take. An address refused its slot gives
/// the permit back as it returns.
fn admit(shared: &Shared, kind: &ListenerKind, peer: SocketAddr) -> Option<HandshakeSlot> {
    let (permits, limit) = match kind {
        ListenerKind::Redirect => (&shared.redirects, MAX_REDIRECTS),
        ListenerKind::Plain | ListenerKind::Tls(_) => (&shared.handshakes, MAX_HANDSHAKES),
    };
    let Ok(permit) = permits.clone().try_acquire_owned() else {
        if let Some(refused) = shared.busy_refusals.should_log() {
            warn!(
                %peer,
                refused,
                "{limit} connections are already in their handshake, so new connections are \
                 being closed (logged at most every {}s)",
                LogThrottle::INTERVAL.as_secs()
            );
        }
        return None;
    };
    let allowance = peer_allowance(permits.available_permits() + 1, limit);
    let Some(peer_slot) = shared.peers.try_acquire(peer.ip(), allowance) else {
        if let Some(refused) = shared.peer_refusals.should_log() {
            warn!(
                %peer,
                refused,
                allowance,
                "a source address already has as many handshakes in progress as it is \
                 allowed while the pool is this full, so its new connections are being \
                 closed (logged at most every {}s)",
                LogThrottle::INTERVAL.as_secs()
            );
        }
        return None;
    };
    Some(HandshakeSlot {
        _permit: permit,
        _peer: peer_slot,
    })
}

/// Accept connections until shutdown, each in its own task.
async fn accept_loop(
    listener: TcpListener,
    kind: ListenerKind,
    shared: Arc<Shared>,
    marker: TaskMarker,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            () = shared.shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!(error = %e, "accepting a connection failed");
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                continue;
            }
        };
        let Some(slot) = admit(&shared, &kind, peer) else {
            drop(stream);
            continue;
        };
        tokio::spawn(handle_connection(
            stream,
            peer,
            kind.clone(),
            shared.clone(),
            slot,
            marker.clone(),
        ));
    }
    debug!("listener closed");
}

/// Serve one accepted connection. `slot` is released when the handshake
/// ends, whatever its result; `_marker` when the connection ends.
async fn handle_connection(
    mut tcp: TcpStream,
    peer: SocketAddr,
    kind: ListenerKind,
    shared: Arc<Shared>,
    slot: HandshakeSlot,
    _marker: TaskMarker,
) {
    if let Err(e) = tcp.set_nodelay(true) {
        debug!(%peer, error = %e, "setting TCP_NODELAY failed");
    }
    let timeouts = shared.timeouts;
    match kind {
        ListenerKind::Redirect => {
            let result = async {
                timed(
                    timeouts.first_stage,
                    "reading the link message",
                    read_link_mess(&mut tcp),
                )
                .await?;
                timed(
                    timeouts.stage,
                    "sending NeedSecured",
                    send_need_secured(&mut tcp),
                )
                .await
            }
            .await;
            drop(slot);
            match result {
                Ok(()) => debug!(%peer, "redirected a client to the TLS listener"),
                Err(e) => debug!(%peer, error = format!("{e:#}"), "redirect failed"),
            }
        }
        ListenerKind::Plain => {
            let stream = SpiceStream::Plain(tcp);
            serve(stream, peer, &shared, slot).await
        }
        ListenerKind::Tls(acceptor) => {
            match tokio::time::timeout(timeouts.first_stage, acceptor.accept(tcp)).await {
                Ok(Ok(tls)) => {
                    let stream = SpiceStream::TlsServer(tls);
                    serve(stream, peer, &shared, slot).await
                }
                Ok(Err(e)) => debug!(%peer, error = %e, "TLS accept failed"),
                Err(_) => debug!(%peer, "TLS accept timed out"),
            }
        }
    }
}

/// Link and authenticate a connection, then serve its channel.
async fn serve(mut stream: SpiceStream, peer: SocketAddr, shared: &Shared, slot: HandshakeSlot) {
    let ctx = HandshakeContext {
        registry: &shared.registry,
        ticket: &shared.ticket,
        timeouts: shared.timeouts,
        key_generations: &shared.key_generations,
        wrong_tickets: &shared.wrong_tickets,
        stale_sessions: &shared.stale_sessions,
        peer,
        playback: shared.platform.audio.is_some(),
    };
    let linked = link::handshake(&mut stream, &ctx).await;
    drop(slot);
    match linked {
        Ok(Some(linked)) => {
            debug!(
                %peer,
                channel = linked.channel.name(),
                session = linked.session.id(),
                "channel linked"
            );
            run_channel(stream, linked, shared).await;
        }
        Ok(None) => {}
        Err(e) => debug!(%peer, error = format!("{e:#}"), "handshake failed"),
    }
}

/// Ends a session when its main channel's task ends, however it ends.
struct EndSessionOnDrop<'a> {
    registry: &'a SessionRegistry,
    session: Arc<Session>,
}

impl Drop for EndSessionOnDrop<'_> {
    fn drop(&mut self) {
        self.registry.end(&self.session);
    }
}

/// Serve a linked channel until it closes, its session ends, or a later
/// link of the same type replaces it.
async fn run_channel(stream: SpiceStream, linked: Linked, shared: &Shared) {
    let session = linked.session.clone();
    let channel = linked.channel;
    let claim = session.claim_channel(channel);
    tokio::select! {
        biased;
        () = claim.replaced() => debug!(
            session = session.id(),
            channel = channel.name(),
            "a later link replaced this channel; closing it"
        ),
        () = session.secondary_channels_closed(), if channel != ServedChannel::Main => debug!(
            session = session.id(),
            channel = channel.name(),
            "the session was replaced; closing the channel"
        ),
        () = serve_channel(stream, linked, shared) => {}
    }
}

/// Drive a linked channel's handler.
async fn serve_channel(stream: SpiceStream, linked: Linked, shared: &Shared) {
    let session = linked.session;
    let ctx = ChannelContext {
        session: session.clone(),
        platform: shared.platform.clone(),
        caps: linked.caps,
        channel_id: linked.channel_id,
        display: shared.display,
    };
    match linked.channel {
        ServedChannel::Main => {
            let _end = EndSessionOnDrop {
                registry: &shared.registry,
                session: session.clone(),
            };
            // Dropped before `_end`, so the summary precedes "session
            // ends", and after `drive`, so it sees how main closed.
            let _summary = main_channel::SessionSummary::new(session.clone());
            let conn = ChannelConn::<MainOpcode>::new(stream, session.clone());
            // A replaced session ends when its main channel has said
            // goodbye, or after FAREWELL_TIMEOUT if it has not.
            let overdue = async {
                session.replaced().await;
                tokio::time::sleep(main_channel::FAREWELL_TIMEOUT).await;
            };
            tokio::select! {
                () = drive(ctx, conn, main_channel::run) => {}
                () = overdue => info!(
                    session = session.id(),
                    "the replaced client did not close in time; ending its session"
                ),
            }
        }
        ServedChannel::Display => {
            let conn = ChannelConn::<DisplayOpcode>::new(stream, session);
            drive(ctx, conn, display::run).await;
        }
        ServedChannel::Inputs => {
            let conn = ChannelConn::<InputsOpcode>::new(stream, session);
            drive(ctx, conn, inputs::run).await;
        }
        ServedChannel::Cursor => {
            let conn = ChannelConn::<CursorOpcode>::new(stream, session);
            drive(ctx, conn, cursor::run).await;
        }
        ServedChannel::Playback => {
            playback::limit_unsent(&stream, playback::Mode::for_caps(&ctx.caps));
            let conn = ChannelConn::<PlaybackOpcode>::new(stream, session);
            drive(ctx, conn, playback::run).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_slots_cap_each_address() {
        let slots = Arc::new(PeerSlots::default());
        let one: IpAddr = "192.0.2.1".parse().unwrap();
        let two: IpAddr = "192.0.2.2".parse().unwrap();
        let held: Vec<PeerSlot> = (0..MAX_HANDSHAKES_PER_PEER)
            .map(|_| {
                slots
                    .try_acquire(one, MAX_HANDSHAKES_PER_PEER)
                    .expect("a slot within the allowance")
            })
            .collect();
        assert!(slots.try_acquire(one, MAX_HANDSHAKES_PER_PEER).is_none());
        assert_eq!(slots.in_progress(one), MAX_HANDSHAKES_PER_PEER);
        let other = slots
            .try_acquire(two, MAX_HANDSHAKES_PER_PEER)
            .expect("another address is unaffected");
        assert_eq!(slots.in_progress(two), 1);

        drop(held);
        assert_eq!(slots.in_progress(one), 0);
        assert!(slots.try_acquire(one, MAX_HANDSHAKES_PER_PEER).is_some());
        drop(other);
        assert!(slots.counts.lock().unwrap().is_empty(), "idle entries go");
    }

    #[test]
    fn a_quiet_pool_allows_a_client_every_parallel_link() {
        assert_eq!(
            peer_allowance(MAX_HANDSHAKES, MAX_HANDSHAKES),
            MAX_HANDSHAKES_PER_PEER
        );
        // And still does while that client's own links are in progress,
        // which is the case that must not regress.
        assert_eq!(
            peer_allowance(MAX_HANDSHAKES - MAX_HANDSHAKES_PER_PEER, MAX_HANDSHAKES),
            MAX_HANDSHAKES_PER_PEER
        );
    }

    #[test]
    fn the_allowance_falls_to_one_slot_as_the_pool_fills() {
        assert_eq!(
            peer_allowance(MAX_HANDSHAKES / 2, MAX_HANDSHAKES),
            MAX_HANDSHAKES_PER_PEER,
            "half free is still the ordinary case"
        );
        assert!(peer_allowance(MAX_HANDSHAKES / 4, MAX_HANDSHAKES) < MAX_HANDSHAKES_PER_PEER);
        assert_eq!(peer_allowance(1, MAX_HANDSHAKES), 1);
        assert_eq!(peer_allowance(0, MAX_HANDSHAKES), 1, "never zero");
    }

    /// The property the allowance exists for, stated as what it costs an
    /// attacker: how many source addresses are needed to hold every
    /// permit, when each address takes everything it is allowed before
    /// the next one starts. A flat cap would need only
    /// `MAX_HANDSHAKES / MAX_HANDSHAKES_PER_PEER` of them -- two, with
    /// the pool of eight and cap of four this replaced.
    #[test]
    fn holding_the_whole_pool_costs_more_addresses_than_a_flat_cap() {
        let mut held = [0usize; 256];
        for used in 0..MAX_HANDSHAKES {
            let allowance = peer_allowance(MAX_HANDSHAKES - used, MAX_HANDSHAKES);
            let next = held
                .iter()
                .position(|&n| n < allowance)
                .expect("an address with room left");
            held[next] += 1;
        }
        assert_eq!(
            held.iter().sum::<usize>(),
            MAX_HANDSHAKES,
            "the pool was filled"
        );
        let addresses = held.iter().filter(|&&n| n > 0).count();
        let flat_cap = MAX_HANDSHAKES / MAX_HANDSHAKES_PER_PEER;
        assert!(
            addresses > flat_cap,
            "{addresses} addresses filled the pool, no better than the {flat_cap} \
             a flat cap of {MAX_HANDSHAKES_PER_PEER} would have cost"
        );
        // The comparison above holds at any pool size -- the allowance
        // beats a flat cap even with the pool of eight this replaced,
        // which cost four addresses instead of two. So the absolute cost
        // is asserted too, because that is the number `docs/running.md`
        // quotes to an operator deciding where to expose andris: about
        // thirty addresses. Shrinking MAX_HANDSHAKES makes that false,
        // and should fail here.
        assert!(
            addresses >= 24,
            "{addresses} addresses filled the pool of {MAX_HANDSHAKES}; \
             docs/running.md says about thirty"
        );
    }

    #[test]
    fn peer_keys() {
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        assert_eq!(peer_key(mapped), "192.0.2.1".parse::<IpAddr>().unwrap());
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(peer_key(a), peer_key(b), "one /64");
        assert_ne!(peer_key(a), peer_key(c));
    }

    #[test]
    fn log_throttle_counts_between_logs() {
        let throttle = LogThrottle::default();
        assert_eq!(throttle.should_log(), Some(1));
        assert_eq!(throttle.should_log(), None);
        assert_eq!(throttle.should_log(), None);
        // Pretend the interval has passed.
        throttle.state.lock().unwrap().0 = Instant::now().checked_sub(LogThrottle::INTERVAL);
        assert_eq!(throttle.should_log(), Some(3));
    }
}
