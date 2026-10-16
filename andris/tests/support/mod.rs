//! Shared test support: an in-process andris on the synthetic platform,
//! and a SPICE client built on ryll's client-side link drivers.
//!
//! Every wait has a timeout, so a broken server fails a test rather
//! than hanging it.

// Each test crate uses only some of this module.
#![allow(dead_code)]
// Test support, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

pub mod display;

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use andris::config::Ticket;
use andris::platform::Size;
use andris::platform::synthetic::SyntheticPlatform;
use andris::server::{
    DisplayOptions, HandshakeTimeouts, Server, ServerHandle, ServerOptions, TlsOptions,
};
use shakenfist_spice_protocol::link::{
    SpiceLinkMess, SpiceLinkReply, SpiceStream, default_channel_caps, perform_auth,
    perform_link_with_caps,
};
use shakenfist_spice_protocol::messages::{ReceiveBuffer, ReceivedMessage, make_message};
use shakenfist_spice_protocol::{ChannelType, SpiceError, capabilities};
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpSocket, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// The ticket every test server uses.
pub const TICKET: &str = "andris-test-ticket";

/// How long any one wait may take. Generous, because RSA key generation
/// in a debug build is slow, and several may run at once.
pub const WAIT: Duration = Duration::from_secs(20);

/// The frame interval of a server from [`TestServer::start`].
pub const FRAME_INTERVAL: Duration = Duration::from_millis(50);

/// The largest message body the test client accepts.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Send the server's logs to the test's output, filtered by `RUST_LOG`.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

/// The display options of a test server unless a test says otherwise:
/// andris's own, but for [`FRAME_INTERVAL`].
pub fn display_options() -> DisplayOptions {
    DisplayOptions {
        frame_interval: FRAME_INTERVAL,
        ..DisplayOptions::default()
    }
}

/// An andris serving a synthetic desktop on loopback. Dropping it shuts
/// the server down.
pub struct TestServer {
    pub handle: ServerHandle,
    pub platform: Arc<SyntheticPlatform>,
    /// The TLS certificate, for clients to trust, if there is a TLS
    /// listener.
    pub certificate: Option<CertificateDer<'static>>,
}

impl TestServer {
    /// A server with only a plain listener.
    pub async fn start() -> Self {
        Self::start_with(false, FRAME_INTERVAL, HandshakeTimeouts::default()).await
    }

    /// A server with only a plain listener, starting display frames at
    /// most every `frame_interval`.
    pub async fn start_with_frame_interval(frame_interval: Duration) -> Self {
        Self::start_with(false, frame_interval, HandshakeTimeouts::default()).await
    }

    /// A server with only a plain listener, starting display frames at
    /// most every `frame_interval`, whose scrub re-reads the whole
    /// desktop every `scrub_period`.
    pub async fn start_with_intervals(frame_interval: Duration, scrub_period: Duration) -> Self {
        let display = DisplayOptions {
            frame_interval,
            scrub_period,
            ..display_options()
        };
        Self::start_full(false, display, HandshakeTimeouts::default()).await
    }

    /// A server with only a plain listener, whose display channels run
    /// with `display`.
    pub async fn start_tuned(display: DisplayOptions) -> Self {
        Self::start_full(false, display, HandshakeTimeouts::default()).await
    }

    /// A server with only a plain listener, exporting a synthetic
    /// desktop of `size`, whose display channels run with `display`.
    pub async fn start_sized(size: Size, display: DisplayOptions) -> Self {
        Self::start_on(false, size, display, HandshakeTimeouts::default()).await
    }

    /// A server with only a plain listener and the handshake time limits
    /// `timeouts`.
    pub async fn start_with_timeouts(timeouts: HandshakeTimeouts) -> Self {
        Self::start_with(false, FRAME_INTERVAL, timeouts).await
    }

    /// A server with a TLS listener, whose plain listener redirects.
    pub async fn start_tls() -> Self {
        Self::start_with(true, FRAME_INTERVAL, HandshakeTimeouts::default()).await
    }

    /// A server with a TLS listener and the handshake time limits
    /// `timeouts`.
    pub async fn start_tls_with_timeouts(timeouts: HandshakeTimeouts) -> Self {
        Self::start_with(true, FRAME_INTERVAL, timeouts).await
    }

    async fn start_with(tls: bool, frame_interval: Duration, timeouts: HandshakeTimeouts) -> Self {
        let display = DisplayOptions {
            frame_interval,
            ..display_options()
        };
        Self::start_full(tls, display, timeouts).await
    }

    async fn start_full(tls: bool, display: DisplayOptions, timeouts: HandshakeTimeouts) -> Self {
        let size = Size {
            width: 256,
            height: 192,
        };
        Self::start_on(tls, size, display, timeouts).await
    }

    async fn start_on(
        tls: bool,
        size: Size,
        display: DisplayOptions,
        timeouts: HandshakeTimeouts,
    ) -> Self {
        init_logging();
        let platform = SyntheticPlatform::new(size).unwrap();
        let (tls_options, certificate) = if tls {
            // Through PEM files, so the server's own loader is exercised.
            let generated =
                rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
            let cert_file = pem_file(&generated.cert.pem());
            let key_file = pem_file(&generated.signing_key.serialize_pem());
            let config =
                andris::server::tls::load_server_config(cert_file.path(), key_file.path()).unwrap();
            let options = TlsOptions {
                listen: "127.0.0.1:0".parse().unwrap(),
                config,
            };
            (Some(options), Some(generated.cert.der().clone()))
        } else {
            (None, None)
        };
        let options = ServerOptions {
            listen: "127.0.0.1:0".parse().unwrap(),
            tls: tls_options,
            ticket: Ticket::new(TICKET).unwrap(),
            display,
            timeouts,
        };
        let handle = Server::bind(options, platform.platform())
            .await
            .unwrap()
            .spawn()
            .unwrap();
        Self {
            handle,
            platform,
            certificate,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.handle.plain_addr()
    }

    pub fn tls_addr(&self) -> SocketAddr {
        self.handle.tls_addr().unwrap()
    }

    /// The current session's id; panics if there is none.
    pub fn session_id(&self) -> u32 {
        self.handle.session_id().expect("there is no session")
    }

    /// Wait until the server has no session.
    pub async fn wait_for_no_session(&self) {
        tokio::time::timeout(WAIT, async {
            while self.handle.session_id().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the session did not end");
    }

    /// A client on the plain listener, with an authenticated channel.
    pub async fn open(&self, connection_id: u32, channel_type: ChannelType) -> TestClient {
        self.open_with_caps(
            connection_id,
            channel_type,
            default_channel_caps(channel_type),
        )
        .await
    }

    /// A client on the plain listener, with an authenticated channel
    /// whose link advertised `channel_caps` as its channel capabilities.
    pub async fn open_with_caps(
        &self,
        connection_id: u32,
        channel_type: ChannelType,
        channel_caps: u32,
    ) -> TestClient {
        let mut client = TestClient::connect(self.addr()).await;
        let reply = client
            .link_with_caps(connection_id, channel_type, channel_caps)
            .await;
        assert_eq!(reply.error, SpiceError::Ok, "linking {channel_type:?}");
        client.auth(TICKET).await.unwrap();
        client
    }

    /// A main channel, and the session it began.
    pub async fn open_main(&self) -> (TestClient, u32) {
        let client = self.open(0, ChannelType::Main).await;
        (client, self.session_id())
    }
}

/// A TCP connection to `addr` from `source`.
pub async fn tcp_from(addr: SocketAddr, source: IpAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(source, 0)).unwrap();
    tokio::time::timeout(WAIT, socket.connect(addr))
        .await
        .unwrap()
        .unwrap()
}

/// The loopback address 127.0.0.`n`.
pub fn loopback(n: u8) -> IpAddr {
    IpAddr::from([127, 0, 0, n])
}

/// Whether the server has closed `stream` within `wait`: a read returns
/// end of file or an error, rather than data or nothing.
pub async fn closed_within<S>(stream: &mut S, wait: Duration) -> bool
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0u8; 1024];
    tokio::time::timeout(wait, async {
        loop {
            match AsyncReadExt::read(stream, &mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await
    .is_ok()
}

/// Wait until `check` holds, polling.
pub async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

fn pem_file(contents: &str) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file
}

/// A SPICE client for one channel.
pub struct TestClient {
    stream: SpiceStream,
    buf: ReceiveBuffer,
    /// The public key from the last successful link reply.
    last_key: Option<Vec<u8>>,
}

impl TestClient {
    /// Connect over plain TCP.
    pub async fn connect(addr: SocketAddr) -> Self {
        let tcp = tokio::time::timeout(WAIT, TcpStream::connect(addr))
            .await
            .unwrap()
            .unwrap();
        Self {
            stream: SpiceStream::Plain(tcp),
            buf: ReceiveBuffer::new(),
            last_key: None,
        }
    }

    /// Connect over plain TCP from `source`, a loopback address other
    /// than 127.0.0.1 (all of 127.0.0.0/8 is loopback), so that tests can
    /// stand in for several peers.
    pub async fn connect_from(addr: SocketAddr, source: IpAddr) -> Self {
        Self {
            stream: SpiceStream::Plain(tcp_from(addr, source).await),
            buf: ReceiveBuffer::new(),
            last_key: None,
        }
    }

    /// Connect over TLS, trusting only `certificate`.
    pub async fn connect_tls(addr: SocketAddr, certificate: &CertificateDer<'static>) -> Self {
        let mut roots = RootCertStore::empty();
        roots.add(certificate.clone()).unwrap();
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let tcp = tokio::time::timeout(WAIT, TcpStream::connect(addr))
            .await
            .unwrap()
            .unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        let tls = tokio::time::timeout(WAIT, connector.connect(name, tcp))
            .await
            .unwrap()
            .unwrap();
        Self {
            stream: SpiceStream::Tls(tls),
            buf: ReceiveBuffer::new(),
            last_key: None,
        }
    }

    /// Send a link message with ryll's default capabilities and return
    /// the server's reply.
    pub async fn link(&mut self, connection_id: u32, channel_type: ChannelType) -> SpiceLinkReply {
        self.link_with_caps(
            connection_id,
            channel_type,
            default_channel_caps(channel_type),
        )
        .await
    }

    /// Send a link message with ryll's default common capabilities and
    /// `channel_caps` as the channel's, and return the server's reply.
    pub async fn link_with_caps(
        &mut self,
        connection_id: u32,
        channel_type: ChannelType,
        channel_caps: u32,
    ) -> SpiceLinkReply {
        let common = [capabilities::DEFAULT_COMMON];
        let channel = [channel_caps];
        let reply = perform_link_with_caps(
            &mut self.stream,
            connection_id,
            channel_type,
            0,
            &common,
            &channel,
        );
        let reply = tokio::time::timeout(WAIT, reply).await.unwrap().unwrap();
        if reply.error == SpiceError::Ok {
            // What ryll's client checks before it authenticates.
            reply.check_client_requirements().unwrap();
            self.last_key = Some(reply.pub_key.clone());
        }
        reply
    }

    /// Send `link` as it is, which ryll's drivers would refuse to send,
    /// and return the server's reply.
    pub async fn link_raw(&mut self, link: &SpiceLinkMess) -> SpiceLinkReply {
        tokio::time::timeout(WAIT, async {
            self.stream.write_all(&link.serialize()).await.unwrap();
            self.stream.flush().await.unwrap();
            let mut header = [0u8; 16];
            self.stream.read_exact(&mut header).await.unwrap();
            let size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            let mut reply = header.to_vec();
            reply.resize(16 + size, 0);
            self.stream.read_exact(&mut reply[16..]).await.unwrap();
            SpiceLinkReply::parse(&reply).unwrap()
        })
        .await
        .unwrap()
    }

    /// Authenticate with `password`, after a successful link. The error
    /// names the server's verdict when it is not `Ok`.
    pub async fn auth(&mut self, password: &str) -> anyhow::Result<()> {
        tokio::time::timeout(WAIT, async {
            let key = self.last_key.take().expect("auth before a successful link");
            perform_auth(&mut self.stream, &key, Some(password)).await
        })
        .await
        .unwrap()
    }

    /// The connection, for tests that need to read or write it raw.
    pub fn stream_mut(&mut self) -> &mut SpiceStream {
        &mut self.stream
    }

    /// Send one message.
    pub async fn send(&mut self, opcode: u16, body: &[u8]) {
        let message = make_message(opcode, body);
        tokio::time::timeout(WAIT, async {
            self.stream.write_all(&message).await.unwrap();
            self.stream.flush().await.unwrap();
        })
        .await
        .unwrap();
    }

    /// Read one message.
    pub async fn recv(&mut self) -> ReceivedMessage {
        self.recv_within(WAIT).await.expect("no message arrived")
    }

    /// Read one message, or `None` if none arrives within `wait`. A
    /// message that is still arriving when `wait` runs out is kept for
    /// the next read.
    pub async fn recv_within(&mut self, wait: Duration) -> Option<ReceivedMessage> {
        tokio::time::timeout(wait, async {
            loop {
                if let Some(message) = self.buf.take_message(MAX_BODY).unwrap() {
                    return message;
                }
                let mut chunk = [0u8; 16 * 1024];
                let read = AsyncReadExt::read(&mut self.stream, &mut chunk)
                    .await
                    .unwrap();
                assert!(read > 0, "the server closed the channel");
                self.buf.extend_from_slice(&chunk[..read]);
            }
        })
        .await
        .ok()
    }

    /// Wait for the server to close the connection, discarding anything
    /// it sends first.
    pub async fn expect_closed(&mut self) {
        tokio::time::timeout(WAIT, async {
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match AsyncReadExt::read(&mut self.stream, &mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        })
        .await
        .expect("the server did not close the connection");
    }
}
