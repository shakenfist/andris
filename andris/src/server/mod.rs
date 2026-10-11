//! The SPICE server: listeners, link and auth, sessions and channels.
//!
//! - [`link`] links and authenticates one connection;
//! - [`session`] holds the one session andris serves at a time;
//! - [`channel`] frames messages, handles the ones every channel shares,
//!   and keeps the ack window;
//! - [`opcodes`] lists what each channel may send;
//! - [`main_channel`], [`display`], [`inputs`] and [`cursor`] serve the
//!   four channels;
//! - [`frames`] cuts captured frames into the display channel's
//!   `DRAW_COPY` bands;
//! - [`keymap`] decodes the inputs channel's set-1 scancodes to evdev
//!   codes;
//! - [`tls`] loads the TLS listener's certificate.
//!
//! # Tasks
//!
//! Each listener runs one accept task. Each accepted connection gets one
//! task, which links and authenticates it and then serves the channel,
//! owning its socket throughout. A connection holds one of
//! [`MAX_HANDSHAKES`] permits from accept until its handshake ends, so
//! unauthenticated peers can only cause a bounded amount of work; with
//! none free, a new connection is closed at once.

pub mod channel;
pub mod cursor;
pub mod display;
pub mod frames;
pub mod inputs;
pub mod keymap;
pub mod link;
pub mod main_channel;
pub mod opcodes;
pub mod session;
pub mod tls;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use shakenfist_spice_protocol::link::{SpiceStream, read_link_mess, send_need_secured};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tracing::{debug, info, warn};

use crate::config::Ticket;
use crate::platform::Platform;
use channel::{ChannelConn, ChannelContext, drive};
use link::{HANDSHAKE_TIMEOUT, Linked, ServedChannel};
use opcodes::{CursorOpcode, DisplayOpcode, InputsOpcode, MainOpcode};
use session::{Cancel, Session, SessionRegistry};

/// How many connections may be in their handshake at once, across both
/// listeners.
pub const MAX_HANDSHAKES: usize = 8;

/// How often a refused connection is logged at most.
const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// How long to pause after a failed accept, so that running out of file
/// descriptors does not become a busy loop.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

/// The TLS listener.
#[derive(Clone)]
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
    pub frame_interval: Duration,
}

/// Logs refused connections at most once per [`REFUSAL_LOG_INTERVAL`],
/// with how many were refused since the last log.
#[derive(Default)]
struct RefusalLog {
    state: Mutex<(Option<Instant>, u64)>,
}

impl RefusalLog {
    fn refused(&self, peer: SocketAddr) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.1 += 1;
        let now = Instant::now();
        if state
            .0
            .is_none_or(|last| now.duration_since(last) >= REFUSAL_LOG_INTERVAL)
        {
            warn!(
                %peer,
                refused = state.1,
                "{MAX_HANDSHAKES} handshakes are already in progress, so new connections are \
                 being closed (logged at most every {}s)",
                REFUSAL_LOG_INTERVAL.as_secs()
            );
            *state = (Some(now), 0);
        }
    }
}

/// State every task of the server shares.
struct Shared {
    ticket: Ticket,
    platform: Platform,
    frame_interval: Duration,
    registry: SessionRegistry,
    handshakes: Arc<Semaphore>,
    refusals: RefusalLog,
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
                frame_interval: options.frame_interval,
                registry: SessionRegistry::new(),
                handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
                refusals: RefusalLog::default(),
                shutdown: Cancel::new(),
            }),
        })
    }

    /// Start accepting connections.
    pub fn spawn(self) -> Result<ServerHandle> {
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
        tokio::spawn(accept_loop(self.plain, plain_kind, self.shared.clone()));
        Ok(ServerHandle {
            plain_addr,
            tls_addr,
            shared: self.shared,
        })
    }
}

/// A running server. Dropping it shuts the server down.
pub struct ServerHandle {
    plain_addr: SocketAddr,
    tls_addr: Option<SocketAddr>,
    shared: Arc<Shared>,
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

    /// Stop accepting connections and end the current session. Idempotent.
    pub fn shutdown(&self) {
        self.shared.shutdown.cancel();
        self.shared.registry.close();
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Accept connections until shutdown, each in its own task.
async fn accept_loop(listener: TcpListener, kind: ListenerKind, shared: Arc<Shared>) {
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
        let Ok(permit) = shared.handshakes.clone().try_acquire_owned() else {
            shared.refusals.refused(peer);
            drop(stream);
            continue;
        };
        tokio::spawn(handle_connection(
            stream,
            peer,
            kind.clone(),
            shared.clone(),
            permit,
        ));
    }
    debug!("listener closed");
}

/// Serve one accepted connection. `permit` is released when the
/// handshake ends, whatever its result.
async fn handle_connection(
    mut tcp: TcpStream,
    peer: SocketAddr,
    kind: ListenerKind,
    shared: Arc<Shared>,
    permit: OwnedSemaphorePermit,
) {
    if let Err(e) = tcp.set_nodelay(true) {
        debug!(%peer, error = %e, "setting TCP_NODELAY failed");
    }
    match kind {
        ListenerKind::Redirect => {
            let result = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
                read_link_mess(&mut tcp).await?;
                send_need_secured(&mut tcp).await
            })
            .await;
            drop(permit);
            match result {
                Ok(Ok(())) => debug!(%peer, "redirected a client to the TLS listener"),
                Ok(Err(e)) => debug!(%peer, error = format!("{e:#}"), "redirect failed"),
                Err(_) => debug!(%peer, "redirect timed out"),
            }
        }
        ListenerKind::Plain => serve(SpiceStream::Plain(tcp), peer, &shared, permit).await,
        ListenerKind::Tls(acceptor) => {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(tls)) => serve(SpiceStream::TlsServer(tls), peer, &shared, permit).await,
                Ok(Err(e)) => debug!(%peer, error = %e, "TLS accept failed"),
                Err(_) => debug!(%peer, "TLS accept timed out"),
            }
        }
    }
}

/// Link and authenticate a connection, then serve its channel.
async fn serve(
    mut stream: SpiceStream,
    peer: SocketAddr,
    shared: &Shared,
    permit: OwnedSemaphorePermit,
) {
    let linked = link::handshake(&mut stream, &shared.registry, &shared.ticket).await;
    drop(permit);
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

/// Serve a linked channel until it closes or its session ends.
async fn run_channel(stream: SpiceStream, linked: Linked, shared: &Shared) {
    let session = linked.session;
    let ctx = ChannelContext {
        session: session.clone(),
        platform: shared.platform.clone(),
        caps: linked.caps,
        channel_id: linked.channel_id,
        frame_interval: shared.frame_interval,
    };
    match linked.channel {
        ServedChannel::Main => {
            let _end = EndSessionOnDrop {
                registry: &shared.registry,
                session: session.clone(),
            };
            let conn = ChannelConn::<MainOpcode>::new(stream, session);
            drive(ctx, conn, main_channel::run).await;
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
    }
}
