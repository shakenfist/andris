//! The guest agent andris plays on the main channel: its flow control,
//! capabilities, resizing and clipboard, against an in-process andris on
//! the synthetic platform.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use andris::platform::{DisplayGeometry, Size};
use andris::server::agent::{MAX_AGENT_MESSAGE, MAX_CHUNK};
use shakenfist_spice_protocol::constants::vd_agent::{
    VD_AGENT_ANNOUNCE_CAPABILITIES, VD_AGENT_CAP_CLIPBOARD_BY_DEMAND,
    VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL, VD_AGENT_CAP_CLIPBOARD_SELECTION,
    VD_AGENT_CAP_GUEST_LINEEND_LF, VD_AGENT_CAP_MONITORS_CONFIG, VD_AGENT_CAP_MOUSE_STATE,
    VD_AGENT_CAP_REPLY, VD_AGENT_CLIPBOARD, VD_AGENT_CLIPBOARD_GRAB, VD_AGENT_CLIPBOARD_IMAGE_PNG,
    VD_AGENT_CLIPBOARD_NONE, VD_AGENT_CLIPBOARD_REQUEST, VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
    VD_AGENT_CLIPBOARD_SELECTION_PRIMARY, VD_AGENT_CLIPBOARD_UTF8_TEXT,
    VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE, VD_AGENT_ERROR, VD_AGENT_MONITORS_CONFIG,
    VD_AGENT_REPLY, VD_AGENT_SUCCESS,
};
use shakenfist_spice_protocol::messages::agent_stream::{
    AgentMessage, AgentReassembler, AgentStreamItem,
};
use shakenfist_spice_protocol::messages::vd_agent::{
    AnnounceCapabilities, Clipboard, ClipboardGrab, ClipboardRequest, ClipboardWireType, MonConfig,
    MonitorMm, MonitorsConfig, VdAgentMessageHeader, VdAgentReply,
};
use shakenfist_spice_protocol::messages::{AgentTokens, MainInit, WireType, make_message};
use shakenfist_spice_protocol::{main_client, main_server};
use tracing_subscriber::fmt::MakeWriter;

use support::{TestClient, TestServer, WAIT, wait_until};

/// How long to watch for a message that should not come.
const QUIET: Duration = Duration::from_millis(300);

fn body<T: WireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write(&mut out);
    out
}

/// A clipboard message body, with the selection header ryll's caps ask
/// for.
fn clip<T: ClipboardWireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write_with(&mut out, true);
    out
}

/// An agent message as one byte stream: header, then `payload`.
fn agent_stream(message_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = body(&VdAgentMessageHeader::new(
        message_type,
        payload.len() as u32,
    ));
    out.extend_from_slice(payload);
    out
}

/// The capabilities ryll announces: asking for the agent's, with
/// `MOUSE_STATE`, `MONITORS_CONFIG`, `REPLY`, `CLIPBOARD_BY_DEMAND` and
/// `CLIPBOARD_SELECTION`.
fn ryll_caps(request: u32) -> AnnounceCapabilities {
    let mut caps = AnnounceCapabilities {
        request,
        caps: Vec::new(),
    };
    for cap in [
        VD_AGENT_CAP_MOUSE_STATE,
        VD_AGENT_CAP_MONITORS_CONFIG,
        VD_AGENT_CAP_REPLY,
        VD_AGENT_CAP_CLIPBOARD_BY_DEMAND,
        VD_AGENT_CAP_CLIPBOARD_SELECTION,
    ] {
        caps.set_capability(cap);
    }
    caps
}

fn monitors_config(width: u32, height: u32) -> MonitorsConfig {
    // With the physical-size table, as spice-gtk always sends it.
    MonitorsConfig {
        flags: VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE,
        monitors: vec![MonConfig {
            height,
            width,
            depth: 32,
            x: 0,
            y: 0,
        }],
        physical_sizes: vec![MonitorMm {
            height: 200,
            width: 300,
        }],
    }
}

fn text_of(len: usize) -> String {
    (0..len)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect()
}

/// What one server message was, to an [`AgentClient`].
#[derive(Debug, PartialEq, Eq)]
enum Got {
    /// An `AGENT_DATA` of this many bytes.
    Chunk(usize),
    /// An `AGENT_TOKEN` handing back this many.
    Tokens(u32),
    /// Anything else.
    Other(u16),
}

/// A main channel that speaks the agent protocol as a compliant client.
struct AgentClient {
    client: TestClient,
    reassembler: AgentReassembler,
    /// Agent messages complete and not yet taken.
    messages: VecDeque<AgentMessage>,
    /// Chunks this client may still send.
    tokens: u32,
    /// Each `AGENT_TOKEN` count received.
    returned: Vec<u32>,
}

impl AgentClient {
    /// Open a main channel, read `INIT` and the first `PING`, and send
    /// `AGENT_START` granting `credit`.
    async fn start(server: &TestServer, credit: u32) -> Self {
        let (mut client, _) = server.open_main().await;
        let init = client.recv().await;
        assert_eq!(init.header.message_type, main_server::INIT);
        let init = MainInit::decode(init.payload()).unwrap();
        assert_eq!(init.agent_connected, 1);
        assert_eq!(client.recv().await.header.message_type, main_server::PING);
        client
            .send(
                main_client::AGENT_START,
                &body(&AgentTokens { num_tokens: credit }),
            )
            .await;
        Self {
            client,
            reassembler: AgentReassembler::default(),
            messages: VecDeque::new(),
            tokens: init.agent_tokens,
            returned: Vec::new(),
        }
    }

    /// [`AgentClient::start`] with ryll's unlimited credit, then read
    /// andris's announcement and announce ryll's capabilities, asking for
    /// nothing back.
    async fn ready(server: &TestServer) -> Self {
        let mut agent = Self::start(server, u32::MAX).await;
        agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;
        agent
            .send_agent(VD_AGENT_ANNOUNCE_CAPABILITIES, &body(&ryll_caps(0)))
            .await;
        agent
    }

    /// Read and account for one server message, or `None` if none comes
    /// within `wait`.
    async fn next(&mut self, wait: Duration) -> Option<Got> {
        let message = self.client.recv_within(wait).await?;
        Some(match message.header.message_type {
            main_server::AGENT_DATA => {
                let chunk = message.payload();
                assert!(chunk.len() <= MAX_CHUNK, "a {}-byte chunk", chunk.len());
                for item in self.reassembler.push(chunk) {
                    let AgentStreamItem::Message(complete) = item else {
                        panic!("andris sent a bad agent stream: {item:?}");
                    };
                    self.messages.push_back(complete);
                }
                Got::Chunk(chunk.len())
            }
            main_server::AGENT_TOKEN => {
                let tokens = AgentTokens::decode(message.payload()).unwrap().num_tokens;
                self.tokens = self.tokens.checked_add(tokens).unwrap();
                self.returned.push(tokens);
                Got::Tokens(tokens)
            }
            other => Got::Other(other),
        })
    }

    /// The next agent message, which must be of `message_type`; returns
    /// its body.
    async fn expect(&mut self, message_type: u32) -> Vec<u8> {
        let message = tokio::time::timeout(WAIT, async {
            loop {
                if let Some(message) = self.messages.pop_front() {
                    return message;
                }
                self.next(WAIT).await.expect("no agent message arrived");
            }
        })
        .await
        .expect("no agent message arrived");
        assert_eq!(
            message.header.message_type, message_type,
            "{:?}",
            message.header
        );
        message.payload
    }

    /// Assert that no agent data arrives for [`QUIET`].
    async fn expect_quiet(&mut self) {
        while let Some(got) = self.next(QUIET).await {
            assert!(!matches!(got, Got::Chunk(_)), "andris sent agent data");
        }
    }

    /// Send one `AGENT_DATA`, waiting for a token first if need be.
    async fn send_chunk(&mut self, chunk: &[u8]) {
        while self.tokens == 0 {
            self.next(WAIT).await.expect("andris handed back no tokens");
        }
        self.tokens -= 1;
        self.client.send(main_client::AGENT_DATA, chunk).await;
    }

    /// Send agent message `message_type` in chunks of at most `MAX_CHUNK`
    /// bytes, one per token, as a compliant client does.
    async fn send_agent(&mut self, message_type: u32, payload: &[u8]) {
        for chunk in agent_stream(message_type, payload).chunks(MAX_CHUNK) {
            self.send_chunk(chunk).await;
        }
    }

    /// Hand andris `tokens` more.
    async fn grant(&mut self, tokens: u32) {
        self.client
            .send(
                main_client::AGENT_TOKEN,
                &body(&AgentTokens { num_tokens: tokens }),
            )
            .await;
    }

    /// Send a monitors config for `width` x `height`, and return whether
    /// andris replied with success.
    async fn resize(&mut self, width: u32, height: u32) -> bool {
        self.send_agent(
            VD_AGENT_MONITORS_CONFIG,
            &body(&monitors_config(width, height)),
        )
        .await;
        let reply = VdAgentReply::decode(&self.expect(VD_AGENT_REPLY).await).unwrap();
        assert_eq!(reply.reply_type, VD_AGENT_MONITORS_CONFIG);
        match reply.error {
            VD_AGENT_SUCCESS => true,
            VD_AGENT_ERROR => false,
            other => panic!("a REPLY with error {other}"),
        }
    }

    /// Wait until andris has handled everything sent so far: a resize to
    /// the current size is answered after it.
    async fn sync(&mut self, server: &TestServer) {
        let size = server.platform.size().unwrap();
        let refusing = !self.resize(size.width, size.height).await;
        assert!(!refusing, "an unchanged size is a success");
    }
}

#[tokio::test]
async fn agent_start_brings_an_announce_that_asks_for_the_clients() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    let caps =
        AnnounceCapabilities::decode(&agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await).unwrap();
    assert_eq!(caps.request, 1);
    for cap in [
        VD_AGENT_CAP_MONITORS_CONFIG,
        VD_AGENT_CAP_REPLY,
        VD_AGENT_CAP_CLIPBOARD_BY_DEMAND,
        VD_AGENT_CAP_CLIPBOARD_SELECTION,
        VD_AGENT_CAP_GUEST_LINEEND_LF,
    ] {
        assert!(caps.has_capability(cap), "capability {cap}");
    }
    // ryll's ClipboardGrab has no serial, so andris must never ask for one.
    assert!(!caps.has_capability(VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL));
    assert!(!caps.has_capability(VD_AGENT_CAP_MOUSE_STATE));
}

#[tokio::test]
async fn a_client_announce_is_answered_only_when_it_asks() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;

    agent
        .send_agent(VD_AGENT_ANNOUNCE_CAPABILITIES, &body(&ryll_caps(1)))
        .await;
    let answer =
        AnnounceCapabilities::decode(&agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await).unwrap();
    assert_eq!(answer.request, 0, "an answer asks for nothing back");
    assert!(answer.has_capability(VD_AGENT_CAP_CLIPBOARD_BY_DEMAND));

    agent
        .send_agent(VD_AGENT_ANNOUNCE_CAPABILITIES, &body(&ryll_caps(0)))
        .await;
    agent.expect_quiet().await;
}

#[tokio::test]
async fn andris_sends_one_chunk_per_client_token() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::start(&server, 0).await;
    agent.expect_quiet().await;

    // The announcement waited for a token.
    agent.grant(1).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;
    agent
        .send_agent(VD_AGENT_ANNOUNCE_CAPABILITIES, &body(&ryll_caps(0)))
        .await;

    // A text that takes three chunks: 20 + 8 + 5000 bytes.
    let text = text_of(5000);
    server.platform.copy_text(Some(&text));
    agent.expect_quiet().await;
    agent.grant(1).await;
    let grab =
        ClipboardGrab::decode_with(&agent.expect(VD_AGENT_CLIPBOARD_GRAB).await, true).unwrap();
    assert!(grab.offers(VD_AGENT_CLIPBOARD_UTF8_TEXT));
    let request = ClipboardRequest {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
    };
    agent
        .send_agent(VD_AGENT_CLIPBOARD_REQUEST, &clip(&request))
        .await;
    agent.expect_quiet().await;

    let mut sizes = Vec::new();
    for _ in 0..3 {
        agent.grant(1).await;
        let size = loop {
            if let Got::Chunk(size) = agent.next(WAIT).await.expect("no chunk arrived") {
                break size;
            }
        };
        sizes.push(size);
        agent.expect_quiet().await;
    }
    assert_eq!(sizes, vec![MAX_CHUNK, MAX_CHUNK, 5028 - 2 * MAX_CHUNK]);
    let data = Clipboard::decode_with(&agent.expect(VD_AGENT_CLIPBOARD).await, true).unwrap();
    assert_eq!(data.clipboard_type, VD_AGENT_CLIPBOARD_UTF8_TEXT);
    assert_eq!(data.data, text.as_bytes());
}

/// A message the client splits over many chunks is reassembled, and the
/// window comes back in batches, so a client that waits for tokens never
/// stalls.
#[tokio::test]
async fn a_split_message_is_reassembled_and_tokens_come_back_in_batches() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::ready(&server).await;

    // 30 chunks: three times the window.
    let text = text_of(30 * MAX_CHUNK - 100);
    let data = Clipboard {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
        data: text.as_bytes().to_vec(),
    };
    agent.send_agent(VD_AGENT_CLIPBOARD, &clip(&data)).await;
    let platform = server.platform.clone();
    wait_until("the text reaches the desktop", || {
        platform.clipboard_set().unwrap().as_deref() == Some(text.as_str())
    })
    .await;
    // Read the last batch back.
    agent.sync(&server).await;
    assert!(
        agent.returned.iter().all(|&tokens| tokens >= 5),
        "{:?}",
        agent.returned
    );
    // 32 chunks went: the announce, the text's 30 and the sync's. Up to
    // a window's worth may still be owed or on its way back.
    let returned: u32 = agent.returned.iter().sum();
    assert!(returned >= 22, "{:?}", agent.returned);
}

#[tokio::test]
async fn agent_data_past_the_window_closes_the_channel() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;

    // Forty one-byte chunks in one write, ignoring the window of ten:
    // andris returns nothing until it has handled what it has read.
    let stream = agent_stream(VD_AGENT_ANNOUNCE_CAPABILITIES, &body(&ryll_caps(0)));
    let mut burst = Vec::new();
    for byte in stream.iter().cycle().take(40) {
        burst.extend(make_message(main_client::AGENT_DATA, &[*byte]));
    }
    let stream = agent.client.stream_mut();
    stream.write_all(&burst).await.unwrap();
    stream.flush().await.unwrap();
    agent.client.expect_closed().await;
    server.wait_for_no_session().await;
}

#[tokio::test]
async fn a_chunk_over_2048_bytes_closes_the_channel() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;
    let mut chunk = agent_stream(VD_AGENT_CLIPBOARD, &[0; MAX_CHUNK]);
    chunk.truncate(MAX_CHUNK + 1);
    agent.send_chunk(&chunk).await;
    agent.client.expect_closed().await;
    server.wait_for_no_session().await;
}

#[tokio::test]
async fn a_header_with_another_protocol_closes_the_channel() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;
    let header = VdAgentMessageHeader {
        protocol: 2,
        ..VdAgentMessageHeader::new(VD_AGENT_MONITORS_CONFIG, 0)
    };
    agent.send_chunk(&body(&header)).await;
    agent.client.expect_closed().await;
}

#[tokio::test]
async fn a_message_over_the_limit_closes_the_channel() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;
    // Only the header: the limit is checked before any body arrives.
    let header = VdAgentMessageHeader::new(VD_AGENT_MONITORS_CONFIG, MAX_AGENT_MESSAGE as u32 + 1);
    agent.send_chunk(&body(&header)).await;
    agent.client.expect_closed().await;
}

#[tokio::test]
async fn a_malformed_agent_start_closes_the_channel() {
    let server = TestServer::start().await;
    let (mut client, _) = server.open_main().await;
    client.recv().await;
    client.recv().await;
    client.send(main_client::AGENT_START, &[1, 2]).await;
    client.expect_closed().await;
}

#[tokio::test]
async fn a_resize_is_applied_or_unchanged() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;

    assert!(agent.resize(320, 200).await);
    assert_eq!(
        server.platform.size().unwrap(),
        Size {
            width: 320,
            height: 200
        }
    );
    assert!(agent.resize(320, 200).await, "unchanged is a success");
    assert!(!agent.resize(0, 200).await, "a size with no pixels");
    assert_eq!(server.platform.size().unwrap().width, 320);
}

/// A writer for a test's own log subscriber, that keeps what it is given.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The server's tasks run on this test's one thread, so a subscriber set
/// for the thread sees what they log.
#[tokio::test(flavor = "current_thread")]
async fn a_refused_resize_is_an_error_reply_logged_once() {
    let server = TestServer::start().await;
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    server.platform.refuse_resizes(true).unwrap();
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;
    for width in [320, 640, 320] {
        assert!(!agent.resize(width, 200).await);
    }
    assert_eq!(
        server.platform.size().unwrap(),
        Size {
            width: 256,
            height: 192
        }
    );
    let text = logs.text();
    assert_eq!(text.matches("the desktop refused").count(), 1, "{text}");
}

#[tokio::test]
async fn the_desktop_clipboard_reaches_the_client() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::ready(&server).await;

    server.platform.copy_text(Some("copied on the desktop"));
    let grab =
        ClipboardGrab::decode_with(&agent.expect(VD_AGENT_CLIPBOARD_GRAB).await, true).unwrap();
    assert_eq!(grab.selection, VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD);
    assert_eq!(grab.types, vec![VD_AGENT_CLIPBOARD_UTF8_TEXT]);
    let request = ClipboardRequest {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
    };
    agent
        .send_agent(VD_AGENT_CLIPBOARD_REQUEST, &clip(&request))
        .await;
    let data = Clipboard::decode_with(&agent.expect(VD_AGENT_CLIPBOARD).await, true).unwrap();
    assert_eq!(data.selection, VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD);
    assert_eq!(data.clipboard_type, VD_AGENT_CLIPBOARD_UTF8_TEXT);
    assert_eq!(data.data, b"copied on the desktop");
}

#[tokio::test]
async fn desktop_text_from_before_the_announce_is_offered_after_it() {
    let server = TestServer::start().await;
    server.platform.copy_text(Some("there all along"));
    let mut agent = AgentClient::start(&server, u32::MAX).await;
    agent.expect(VD_AGENT_ANNOUNCE_CAPABILITIES).await;
    agent.expect_quiet().await;
    agent
        .send_agent(VD_AGENT_ANNOUNCE_CAPABILITIES, &body(&ryll_caps(0)))
        .await;
    agent.expect(VD_AGENT_CLIPBOARD_GRAB).await;
}

#[tokio::test]
async fn the_client_clipboard_reaches_the_desktop() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::ready(&server).await;

    let grab = ClipboardGrab {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        types: vec![VD_AGENT_CLIPBOARD_IMAGE_PNG, VD_AGENT_CLIPBOARD_UTF8_TEXT],
    };
    agent
        .send_agent(VD_AGENT_CLIPBOARD_GRAB, &clip(&grab))
        .await;
    let request =
        ClipboardRequest::decode_with(&agent.expect(VD_AGENT_CLIPBOARD_REQUEST).await, true)
            .unwrap();
    assert_eq!(request.selection, VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD);
    assert_eq!(request.clipboard_type, VD_AGENT_CLIPBOARD_UTF8_TEXT);
    let data = Clipboard {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
        data: "copied on the client".into(),
    };
    agent.send_agent(VD_AGENT_CLIPBOARD, &clip(&data)).await;
    let platform = server.platform.clone();
    wait_until("the text reaches the desktop", || {
        platform.clipboard_set().unwrap().as_deref() == Some("copied on the client")
    })
    .await;

    // A grab with no text is not asked for.
    let grab = ClipboardGrab {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        types: vec![VD_AGENT_CLIPBOARD_IMAGE_PNG],
    };
    agent
        .send_agent(VD_AGENT_CLIPBOARD_GRAB, &clip(&grab))
        .await;
    agent.expect_quiet().await;
}

/// Text that comes back the way it went, as it does when the RDP client
/// and ryll share one machine's clipboard, crosses no further.
#[tokio::test]
async fn an_echo_is_dropped_both_ways() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::ready(&server).await;
    let request = ClipboardRequest {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
    };
    let grab = ClipboardGrab {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        types: vec![VD_AGENT_CLIPBOARD_UTF8_TEXT],
    };
    let text = |text: &str| Clipboard {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
        data: text.into(),
    };

    // Client to desktop, then back: no grab.
    agent
        .send_agent(VD_AGENT_CLIPBOARD_GRAB, &clip(&grab))
        .await;
    agent.expect(VD_AGENT_CLIPBOARD_REQUEST).await;
    agent
        .send_agent(VD_AGENT_CLIPBOARD, &clip(&text("first")))
        .await;
    let platform = server.platform.clone();
    wait_until("the text reaches the desktop", || {
        platform.clipboard_sets().unwrap() == 1
    })
    .await;
    server.platform.copy_text(Some("first"));
    agent.expect_quiet().await;

    // Desktop to client, then back: not set on the desktop again.
    server.platform.copy_text(Some("second"));
    agent.expect(VD_AGENT_CLIPBOARD_GRAB).await;
    agent
        .send_agent(VD_AGENT_CLIPBOARD_REQUEST, &clip(&request))
        .await;
    agent.expect(VD_AGENT_CLIPBOARD).await;
    agent
        .send_agent(VD_AGENT_CLIPBOARD_GRAB, &clip(&grab))
        .await;
    agent.expect(VD_AGENT_CLIPBOARD_REQUEST).await;
    agent
        .send_agent(VD_AGENT_CLIPBOARD, &clip(&text("second")))
        .await;
    agent.sync(&server).await;
    assert_eq!(server.platform.clipboard_sets().unwrap(), 1);
    assert_eq!(
        server.platform.clipboard_set().unwrap().as_deref(),
        Some("first")
    );

    // New text still crosses.
    server.platform.copy_text(Some("third"));
    agent.expect(VD_AGENT_CLIPBOARD_GRAB).await;
}

#[tokio::test]
async fn other_selections_and_types_get_an_empty_clipboard() {
    let server = TestServer::start().await;
    let mut agent = AgentClient::ready(&server).await;
    server.platform.copy_text(Some("text"));
    agent.expect(VD_AGENT_CLIPBOARD_GRAB).await;

    for (selection, clipboard_type) in [
        (
            VD_AGENT_CLIPBOARD_SELECTION_PRIMARY,
            VD_AGENT_CLIPBOARD_UTF8_TEXT,
        ),
        (
            VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            VD_AGENT_CLIPBOARD_IMAGE_PNG,
        ),
    ] {
        let request = ClipboardRequest {
            selection,
            clipboard_type,
        };
        agent
            .send_agent(VD_AGENT_CLIPBOARD_REQUEST, &clip(&request))
            .await;
        let data = Clipboard::decode_with(&agent.expect(VD_AGENT_CLIPBOARD).await, true).unwrap();
        assert_eq!(data.selection, selection);
        assert_eq!(data.clipboard_type, VD_AGENT_CLIPBOARD_NONE);
        assert!(data.data.is_empty());
    }

    // Once the client takes the clipboard, andris has nothing to give.
    let grab = ClipboardGrab {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        types: vec![VD_AGENT_CLIPBOARD_IMAGE_PNG],
    };
    agent
        .send_agent(VD_AGENT_CLIPBOARD_GRAB, &clip(&grab))
        .await;
    let request = ClipboardRequest {
        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
    };
    agent
        .send_agent(VD_AGENT_CLIPBOARD_REQUEST, &clip(&request))
        .await;
    let data = Clipboard::decode_with(&agent.expect(VD_AGENT_CLIPBOARD).await, true).unwrap();
    assert_eq!(data.clipboard_type, VD_AGENT_CLIPBOARD_NONE);
}
