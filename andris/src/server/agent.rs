//! The guest agent, which andris plays itself on the main channel.
//!
//! A SPICE server usually relays the main channel's `AGENT_DATA` to a
//! vdagent running in the guest. Andris runs inside the desktop it
//! exports, so it answers as the agent itself: [`Agent`] holds one main
//! channel's agent state, and the main channel hands it every agent
//! message. `MAIN_INIT` says the agent is connected, and it stays
//! connected for the life of the channel.
//!
//! # Flow control
//!
//! Agent data flows in chunks, each one `AGENT_DATA` of at most
//! [`MAX_CHUNK`] bytes. An agent message is a 20-byte header and a body,
//! cut into chunks as one byte stream, with the header at the start of
//! the first; a message never shares a chunk with another.
//!
//! - **Client to andris.** `MAIN_INIT` grants the client a window of
//!   [`AGENT_WINDOW`] chunks. Each `AGENT_DATA` uses one, and andris hands
//!   them back with `AGENT_TOKEN` once at least [`TOKEN_BATCH`] are owed
//!   and it has handled everything it has read from the client. A
//!   compliant client therefore always holds at least
//!   `AGENT_WINDOW - TOKEN_BATCH + 1` tokens while andris catches up, and
//!   never waits on a partial batch; a client that sends past its window
//!   in one burst is found out, because andris returns nothing until it
//!   has handled the burst.
//! - **Andris to client.** `AGENT_START` sets how many chunks the client
//!   will take, and each client `AGENT_TOKEN` adds to it. Andris sends one
//!   chunk per token and queues the rest, up to [`MAX_QUEUED`] bytes.
//!
//! A chunk with no token left, a chunk over [`MAX_CHUNK`] bytes, a header
//! with the wrong protocol, a malformed message, or a message over
//! [`MAX_AGENT_MESSAGE`] bytes closes the channel, as other malformed
//! input does. The one exception is clipboard data over the limit, which
//! is dropped with a warning: the reassembler skips its body without
//! keeping it.
//!
//! # What the agent does
//!
//! - **Capabilities.** On `AGENT_START` andris announces its
//!   capabilities and asks for the client's, and it answers a client
//!   announcement that asks for andris's. Clipboard messages wait for the
//!   client's announcement, since their layout depends on it.
//! - **Monitors config.** The first monitor's size goes to the platform's
//!   [`DisplayControl`], and the client is told with `REPLY` whether it
//!   was applied. A refusal is logged once per connection.
//! - **Clipboard.** The `CLIPBOARD` selection, as UTF-8 text, both ways.
//!   Andris fetches eagerly: when the client grabs with text, andris asks
//!   for it at once and puts it on the desktop; when the desktop's text
//!   changes, andris grabs on the client and answers its request from the
//!   text it holds. Text identical to the last text that crossed, in
//!   either direction, is dropped, which breaks the loop two clipboard
//!   bridges on one machine would otherwise make. The comparison has
//!   known gaps: text is recorded as crossed when it is handed to the
//!   desktop, so if the desktop then loses the race for the selection to
//!   another program, the same text copied on the client again is taken
//!   for an echo and does not cross until other text has. Text over
//!   [`MAX_CLIPBOARD_TEXT`] is dropped with a warning. A request for
//!   another selection or type is answered with an empty clipboard.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use shakenfist_spice_protocol::constants::vd_agent::{
    VD_AGENT_ANNOUNCE_CAPABILITIES, VD_AGENT_CAP_CLIPBOARD_BY_DEMAND,
    VD_AGENT_CAP_CLIPBOARD_SELECTION, VD_AGENT_CAP_GUEST_LINEEND_LF, VD_AGENT_CAP_MONITORS_CONFIG,
    VD_AGENT_CAP_REPLY, VD_AGENT_CLIPBOARD, VD_AGENT_CLIPBOARD_GRAB, VD_AGENT_CLIPBOARD_NONE,
    VD_AGENT_CLIPBOARD_RELEASE, VD_AGENT_CLIPBOARD_REQUEST, VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
    VD_AGENT_CLIPBOARD_UTF8_TEXT, VD_AGENT_ERROR, VD_AGENT_MAX_DATA_SIZE, VD_AGENT_MONITORS_CONFIG,
    VD_AGENT_REPLY, VD_AGENT_SUCCESS,
};
use shakenfist_spice_protocol::messages::agent_stream::{
    AgentMessage, AgentReassembler, AgentStreamItem,
};
use shakenfist_spice_protocol::messages::vd_agent::{
    AnnounceCapabilities, Clipboard as ClipboardData, ClipboardGrab, ClipboardRelease,
    ClipboardRequest, ClipboardWireType, MonitorsConfig, VdAgentMessageHeader, VdAgentReply,
};
use shakenfist_spice_protocol::messages::{AgentTokens, WireType};
use tracing::{debug, info, warn};

use super::channel::ChannelConn;
use super::opcodes::MainOpcode;
use crate::platform::{
    Clipboard, ClipboardReceiver, DisplayControl, MAX_CLIPBOARD_TEXT, Platform, Resize, Size,
    text_hash,
};

/// The agent chunks `MAIN_INIT` lets the client send before andris hands
/// any back, as spice-server grants.
pub const AGENT_WINDOW: u32 = 10;

/// How many tokens andris owes before it hands them back, as
/// spice-server batches them.
pub const TOKEN_BATCH: u32 = 5;

/// The largest `AGENT_DATA` body, in either direction: 2048 bytes,
/// including the agent message header on a message's first chunk.
pub const MAX_CHUNK: usize = VD_AGENT_MAX_DATA_SIZE as usize;

/// The largest agent message body andris accepts from the client: the
/// clipboard limit, and room for the clipboard message's own fields.
pub const MAX_AGENT_MESSAGE: usize = MAX_CLIPBOARD_TEXT + 4096;

/// The most agent data andris queues for a client that has run out of
/// tokens, in bytes: two messages of the largest size. A message that
/// would take the queue past it is dropped.
pub const MAX_QUEUED: usize = 2 * (MAX_AGENT_MESSAGE + VdAgentMessageHeader::SIZE);

/// The capabilities andris announces as the agent: `request` non-zero
/// asks the client for its own.
fn capabilities(request: u32) -> AnnounceCapabilities {
    let mut caps = AnnounceCapabilities {
        request,
        caps: Vec::new(),
    };
    // Never VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL: it adds a serial to every
    // grab, which ryll's ClipboardGrab does not model, so andris could
    // neither read the client's grabs nor write its own.
    for cap in [
        VD_AGENT_CAP_MONITORS_CONFIG,
        VD_AGENT_CAP_REPLY,
        VD_AGENT_CAP_CLIPBOARD_BY_DEMAND,
        VD_AGENT_CAP_CLIPBOARD_SELECTION,
        VD_AGENT_CAP_GUEST_LINEEND_LF,
    ] {
        caps.set_capability(cap);
    }
    caps
}

/// Agent message `message_type` with `body`, cut into `AGENT_DATA` bodies
/// of at most [`MAX_CHUNK`] bytes, the header leading the first.
fn chunk_message(message_type: u32, body: &[u8]) -> Result<Vec<Vec<u8>>> {
    let size = u32::try_from(body.len())
        .with_context(|| format!("an agent message of {} bytes is too long", body.len()))?;
    let mut stream = Vec::with_capacity(VdAgentMessageHeader::SIZE + body.len());
    VdAgentMessageHeader::new(message_type, size).write(&mut stream);
    stream.extend_from_slice(body);
    Ok(stream.chunks(MAX_CHUNK).map(<[u8]>::to_vec).collect())
}

/// The size a monitors config asks for: its first monitor's. Fails unless
/// there is a monitor whose size has pixels, whose BGRX bytes fit a
/// `u32`, and which andris's frame limits allow.
fn requested_size(config: &MonitorsConfig) -> Result<Size> {
    let first = config
        .monitors
        .first()
        .context("a monitors config with no monitors")?;
    let size = Size {
        width: first.width,
        height: first.height,
    };
    ensure!(
        first
            .width
            .checked_mul(first.height)
            .and_then(|pixels| pixels.checked_mul(4))
            .is_some(),
        "a {size} monitor is too large"
    );
    size.check()?;
    Ok(size)
}

/// The client's agent window, as andris counts it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientWindow {
    /// Chunks the client may still send.
    left: u32,
    /// Chunks handled whose tokens andris has not yet handed back.
    owed: u32,
}

impl ClientWindow {
    fn new() -> Self {
        Self {
            left: AGENT_WINDOW,
            owed: 0,
        }
    }

    /// Account for a chunk arriving. Fails if the client had no token.
    fn take(&mut self) -> Result<()> {
        ensure!(
            self.left > 0,
            "the client sent AGENT_DATA with no agent token left"
        );
        self.left -= 1;
        Ok(())
    }

    /// A chunk has been handled, so its token is owed back.
    fn consumed(&mut self) {
        self.owed = self.owed.saturating_add(1);
    }

    /// Whether a batch is owed.
    fn owes(&self) -> bool {
        self.owed >= TOKEN_BATCH
    }

    /// Hand back every token owed, and return how many.
    fn hand_back(&mut self) -> u32 {
        let owed = std::mem::take(&mut self.owed);
        self.left = self.left.saturating_add(owed);
        owed
    }
}

/// Agent chunks waiting for the client's tokens.
#[derive(Debug, Default)]
struct Outgoing {
    /// Chunks the client will take.
    credit: u32,
    chunks: VecDeque<Vec<u8>>,
    /// The bytes in `chunks`.
    queued: usize,
}

impl Outgoing {
    /// Whether a message with a body of `body` bytes fits in the queue.
    fn has_room(&self, body: usize) -> bool {
        self.queued
            .saturating_add(VdAgentMessageHeader::SIZE)
            .saturating_add(body)
            <= MAX_QUEUED
    }

    /// Queue agent message `message_type` with `body`. Returns false, and
    /// queues nothing, if it would take the queue past [`MAX_QUEUED`].
    fn push(&mut self, message_type: u32, body: &[u8]) -> Result<bool> {
        if !self.has_room(body.len()) {
            return Ok(false);
        }
        let chunks = chunk_message(message_type, body)?;
        let bytes: usize = chunks.iter().map(Vec::len).sum();
        self.queued += bytes;
        self.chunks.extend(chunks);
        Ok(true)
    }

    /// The next chunk to send, spending a token, if there is one and a
    /// token to spend.
    fn next(&mut self) -> Option<Vec<u8>> {
        if self.credit == 0 {
            return None;
        }
        let chunk = self.chunks.pop_front()?;
        self.credit -= 1;
        self.queued = self.queued.saturating_sub(chunk.len());
        Some(chunk)
    }
}

/// One main channel's guest agent. See the module documentation.
pub struct Agent {
    session: u32,
    display_control: Arc<dyn DisplayControl>,
    clipboard: Arc<dyn Clipboard>,
    /// The desktop's clipboard text.
    desktop: ClipboardReceiver,
    window: ClientWindow,
    outgoing: Outgoing,
    reassembler: AgentReassembler,
    /// Whether the client has started the agent.
    started: bool,
    /// The client's capabilities, once it has announced them.
    client_caps: Option<AnnounceCapabilities>,
    /// Whether a refused resize has been logged on this connection.
    refusal_logged: bool,
    /// Whether a message dropped from a full queue has been logged at
    /// warning level on this connection.
    drop_logged: bool,
    /// The last text that crossed the clipboard bridge, either way.
    last_crossed: Option<Arc<str>>,
    /// The desktop text andris grabbed the client's clipboard with, while
    /// that grab stands.
    offered: Option<Arc<str>>,
}

impl Agent {
    /// The agent of session `session`, on `platform`'s desktop.
    pub fn new(session: u32, platform: &Platform) -> Self {
        Self {
            session,
            display_control: platform.display_control.clone(),
            clipboard: platform.clipboard.clone(),
            desktop: platform.clipboard.changes(),
            window: ClientWindow::new(),
            outgoing: Outgoing::default(),
            reassembler: AgentReassembler::new(MAX_AGENT_MESSAGE),
            started: false,
            client_caps: None,
            refusal_logged: false,
            drop_logged: false,
            last_crossed: None,
            offered: None,
        }
    }

    /// Handle `AGENT_START`: the client's credit, after which andris
    /// announces its capabilities. A client starts the agent once per
    /// connection, since andris's agent never disconnects; a second start
    /// is ignored, rather than resetting the credit granted so far or
    /// announcing again.
    pub async fn start(&mut self, conn: &mut ChannelConn<MainOpcode>, body: &[u8]) -> Result<()> {
        let tokens = AgentTokens::decode(body).context("a malformed AGENT_START")?;
        if self.started {
            debug!(
                session = self.session,
                tokens = tokens.num_tokens,
                "ignoring a second AGENT_START"
            );
            return Ok(());
        }
        self.started = true;
        debug!(
            session = self.session,
            tokens = tokens.num_tokens,
            "the client started the agent"
        );
        self.outgoing.credit = tokens.num_tokens;
        self.queue(VD_AGENT_ANNOUNCE_CAPABILITIES, &capabilities(1))?;
        self.flush(conn).await
    }

    /// Handle a client `AGENT_TOKEN`: more credit.
    pub async fn token(&mut self, conn: &mut ChannelConn<MainOpcode>, body: &[u8]) -> Result<()> {
        let tokens = AgentTokens::decode(body).context("a malformed AGENT_TOKEN")?;
        self.outgoing.credit = self.outgoing.credit.saturating_add(tokens.num_tokens);
        self.flush(conn).await
    }

    /// Handle an `AGENT_DATA`: one chunk of the client's agent messages.
    pub async fn data(&mut self, conn: &mut ChannelConn<MainOpcode>, chunk: &[u8]) -> Result<()> {
        ensure!(
            chunk.len() <= MAX_CHUNK,
            "the client sent an AGENT_DATA of {} bytes, over the {MAX_CHUNK}-byte limit",
            chunk.len()
        );
        self.window.take()?;
        for item in self.reassembler.push(chunk) {
            match item {
                AgentStreamItem::Message(message) => self.handle(message).await?,
                AgentStreamItem::Oversized(header) if header.message_type == VD_AGENT_CLIPBOARD => {
                    warn!(
                        session = self.session,
                        bytes = header.size,
                        "dropping the client's clipboard text: it is over the \
                         {MAX_CLIPBOARD_TEXT}-byte limit"
                    );
                }
                AgentStreamItem::Oversized(header) => bail!(
                    "the client sent agent message type {} of {} bytes, over the \
                     {MAX_AGENT_MESSAGE}-byte limit",
                    header.message_type,
                    header.size
                ),
                AgentStreamItem::BadProtocol(header) => bail!(
                    "the client sent an agent message with protocol {}",
                    header.protocol
                ),
            }
        }
        self.window.consumed();
        self.flush(conn).await
    }

    /// Whether andris owes the client a batch of tokens.
    pub fn owes_tokens(&self) -> bool {
        self.window.owes()
    }

    /// Hand the client back the tokens owed.
    pub async fn return_tokens(&mut self, conn: &mut ChannelConn<MainOpcode>) -> Result<()> {
        let num_tokens = self.window.hand_back();
        conn.send_msg(MainOpcode::AgentToken, &AgentTokens { num_tokens })
            .await
    }

    /// Wait until the desktop's clipboard changes. Cancel-safe. Never
    /// returns if the platform can no longer report changes.
    pub async fn desktop_clipboard_changed(&mut self) {
        if self.desktop.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Offer the client the desktop's clipboard as it is now.
    pub async fn desktop_clipboard(&mut self, conn: &mut ChannelConn<MainOpcode>) -> Result<()> {
        let text = self.desktop.borrow_and_update().clone();
        debug!(
            session = self.session,
            bytes = text.as_ref().map_or(0, |text| text.len()),
            hash = text.as_deref().map(text_hash),
            "the desktop's clipboard changed"
        );
        self.offer(text)?;
        self.flush(conn).await
    }

    /// Send as many queued chunks as the client has tokens for.
    async fn flush(&mut self, conn: &mut ChannelConn<MainOpcode>) -> Result<()> {
        while let Some(chunk) = self.outgoing.next() {
            conn.send(MainOpcode::AgentData, &chunk).await?;
        }
        Ok(())
    }

    /// Queue agent message `message_type`; false if it was dropped.
    fn queue<T: WireType>(&mut self, message_type: u32, message: &T) -> Result<bool> {
        let mut body = Vec::new();
        message.write(&mut body);
        self.queue_body(message_type, &body)
    }

    /// Queue clipboard message `message_type`, in the layout the client's
    /// capabilities choose; false if it was dropped.
    fn queue_clipboard<T: ClipboardWireType>(
        &mut self,
        message_type: u32,
        message: &T,
        has_selection: bool,
    ) -> Result<bool> {
        let mut body = Vec::new();
        message.write_with(&mut body, has_selection);
        self.queue_body(message_type, &body)
    }

    /// Queue `body` as agent message `message_type`; false if the queue
    /// had no room for it and it was dropped.
    fn queue_body(&mut self, message_type: u32, body: &[u8]) -> Result<bool> {
        let queued = self.outgoing.push(message_type, body)?;
        if !queued {
            self.dropped(message_type, body.len());
        }
        Ok(queued)
    }

    /// Log a message dropped from a full queue: at warning level once per
    /// connection, since a client that takes nothing could otherwise fill
    /// the log, and at debug level after that.
    fn dropped(&mut self, message_type: u32, bytes: usize) {
        if self.drop_logged {
            debug!(
                session = self.session,
                message_type, bytes, "dropping an agent message: the queue is full"
            );
        } else {
            self.drop_logged = true;
            warn!(
                session = self.session,
                message_type,
                bytes,
                "dropping an agent message: the client has taken too little of what is \
                 queued (logged once per connection)"
            );
        }
    }

    /// Whether clipboard messages carry a selection, or `None` before the
    /// client has announced its capabilities.
    fn has_selection(&self) -> Option<bool> {
        self.client_caps
            .as_ref()
            .map(|caps| caps.has_capability(VD_AGENT_CAP_CLIPBOARD_SELECTION))
    }

    /// Handle one complete agent message from the client.
    async fn handle(&mut self, message: AgentMessage) -> Result<()> {
        let AgentMessage { header, payload } = message;
        match header.message_type {
            VD_AGENT_ANNOUNCE_CAPABILITIES => self.announced(&payload),
            VD_AGENT_MONITORS_CONFIG => self.monitors_config(&payload).await,
            VD_AGENT_CLIPBOARD_GRAB
            | VD_AGENT_CLIPBOARD
            | VD_AGENT_CLIPBOARD_REQUEST
            | VD_AGENT_CLIPBOARD_RELEASE => {
                let Some(has_selection) = self.has_selection() else {
                    debug!(
                        session = self.session,
                        message_type = header.message_type,
                        "ignoring a clipboard message sent before the client's capabilities"
                    );
                    return Ok(());
                };
                self.clipboard_message(header.message_type, &payload, has_selection)
            }
            other => {
                debug!(
                    session = self.session,
                    message_type = other,
                    "ignoring an agent message andris does not handle"
                );
                Ok(())
            }
        }
    }

    /// The client announced its capabilities.
    fn announced(&mut self, body: &[u8]) -> Result<()> {
        let caps = AnnounceCapabilities::decode(body)
            .context("a malformed agent ANNOUNCE_CAPABILITIES")?;
        debug!(
            session = self.session,
            request = caps.request,
            caps = ?caps.caps,
            "the client announced its agent capabilities"
        );
        let request = caps.request;
        self.client_caps = Some(caps);
        if request != 0 {
            self.queue(VD_AGENT_ANNOUNCE_CAPABILITIES, &capabilities(0))?;
        }
        // Clipboard messages can flow now; offer what the desktop holds.
        let text = self.desktop.borrow().clone();
        self.offer(text)
    }

    /// Apply a monitors config, and tell the client whether it was.
    async fn monitors_config(&mut self, body: &[u8]) -> Result<()> {
        let config = MonitorsConfig::decode(body).context("a malformed agent MONITORS_CONFIG")?;
        let applied = match requested_size(&config) {
            Ok(size) => self.resize(size).await,
            Err(error) => {
                debug!(
                    session = self.session,
                    error = format!("{error:#}"),
                    "refusing the client's monitors config"
                );
                false
            }
        };
        let reply = VdAgentReply {
            reply_type: VD_AGENT_MONITORS_CONFIG,
            error: if applied {
                VD_AGENT_SUCCESS
            } else {
                VD_AGENT_ERROR
            },
        };
        self.queue(VD_AGENT_REPLY, &reply)?;
        Ok(())
    }

    /// Ask the desktop to be `size`; true if it now is.
    async fn resize(&mut self, size: Size) -> bool {
        let control = self.display_control.clone();
        let outcome = tokio::task::spawn_blocking(move || control.set_size(size)).await;
        match outcome {
            Ok(Ok(Resize::Applied)) => {
                info!(
                    session = self.session,
                    "resized the desktop to {size}, as the client asked"
                );
                true
            }
            Ok(Ok(Resize::Unchanged)) => {
                debug!(
                    session = self.session,
                    "the desktop is already {size}, as the client asked"
                );
                true
            }
            Ok(Ok(Resize::Refused(why))) => {
                if self.refusal_logged {
                    debug!(
                        session = self.session,
                        "the desktop refused a resize to {size}: {why}"
                    );
                } else {
                    self.refusal_logged = true;
                    warn!(
                        session = self.session,
                        "the client asked for a {size} desktop, and the desktop refused: {why}; \
                         it keeps its size (logged once per connection)"
                    );
                }
                false
            }
            Ok(Err(error)) => {
                warn!(
                    session = self.session,
                    error = format!("{error:#}"),
                    "resizing the desktop to {size} failed"
                );
                false
            }
            Err(error) => {
                warn!(
                    session = self.session,
                    error = %error,
                    "resizing the desktop to {size} failed"
                );
                false
            }
        }
    }

    /// Handle a clipboard message, laid out as `has_selection` says.
    fn clipboard_message(
        &mut self,
        message_type: u32,
        body: &[u8],
        has_selection: bool,
    ) -> Result<()> {
        match message_type {
            VD_AGENT_CLIPBOARD_GRAB => {
                let grab = ClipboardGrab::decode_with(body, has_selection)
                    .context("a malformed agent CLIPBOARD_GRAB")?;
                if grab.selection != VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD {
                    debug!(
                        session = self.session,
                        selection = grab.selection,
                        "ignoring a grab of a selection other than CLIPBOARD"
                    );
                    return Ok(());
                }
                // The client owns the clipboard now, whatever it offers.
                self.offered = None;
                if grab.offers(VD_AGENT_CLIPBOARD_UTF8_TEXT) {
                    let request = ClipboardRequest {
                        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
                        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
                    };
                    self.queue_clipboard(VD_AGENT_CLIPBOARD_REQUEST, &request, has_selection)?;
                } else {
                    debug!(
                        session = self.session,
                        types = ?grab.types,
                        "the client's clipboard holds no text"
                    );
                }
            }
            VD_AGENT_CLIPBOARD => {
                let data = ClipboardData::decode_with(body, has_selection)
                    .context("a malformed agent CLIPBOARD")?;
                if data.selection == VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD
                    && data.clipboard_type == VD_AGENT_CLIPBOARD_UTF8_TEXT
                {
                    self.client_text(data.data);
                } else {
                    debug!(
                        session = self.session,
                        selection = data.selection,
                        clipboard_type = data.clipboard_type,
                        "ignoring clipboard data that is not CLIPBOARD text"
                    );
                }
            }
            VD_AGENT_CLIPBOARD_REQUEST => {
                let request = ClipboardRequest::decode_with(body, has_selection)
                    .context("a malformed agent CLIPBOARD_REQUEST")?;
                let text = self.offered.clone().filter(|_| {
                    request.selection == VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD
                        && request.clipboard_type == VD_AGENT_CLIPBOARD_UTF8_TEXT
                });
                // Before the text is copied into a body: a client asking
                // again and again for large text it does not take would
                // otherwise cost a copy of it each time, only to drop it.
                // The answer's fields are at most 8 bytes besides the text.
                let body = text.as_ref().map_or(0, |text| text.len()).saturating_add(8);
                if !self.outgoing.has_room(body) {
                    self.dropped(VD_AGENT_CLIPBOARD, body);
                    return Ok(());
                }
                debug!(
                    session = self.session,
                    bytes = text.as_ref().map_or(0, |text| text.len()),
                    hash = text.as_deref().map(text_hash),
                    "answering the client's clipboard request"
                );
                let answer = match text {
                    Some(text) => ClipboardData {
                        selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
                        clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
                        data: text.as_bytes().to_vec(),
                    },
                    None => ClipboardData {
                        selection: request.selection,
                        clipboard_type: VD_AGENT_CLIPBOARD_NONE,
                        data: Vec::new(),
                    },
                };
                self.queue_clipboard(VD_AGENT_CLIPBOARD, &answer, has_selection)?;
            }
            VD_AGENT_CLIPBOARD_RELEASE => {
                let release = ClipboardRelease::decode_with(body, has_selection)
                    .context("a malformed agent CLIPBOARD_RELEASE")?;
                debug!(
                    session = self.session,
                    selection = release.selection,
                    "the client released its clipboard"
                );
            }
            other => bail!("agent message type {other} is not a clipboard message"),
        }
        Ok(())
    }

    /// The client sent its clipboard text: put it on the desktop, unless
    /// it is the text that last crossed.
    fn client_text(&mut self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        // Checked after the conversion, since replacing invalid UTF-8 can
        // make the text up to three times longer.
        let text: Arc<str> = match String::from_utf8(data) {
            Ok(text) => Arc::from(text),
            Err(error) => Arc::from(String::from_utf8_lossy(error.as_bytes())),
        };
        if text.len() > MAX_CLIPBOARD_TEXT {
            warn!(
                session = self.session,
                bytes = text.len(),
                "dropping the client's clipboard text: it is over the \
                 {MAX_CLIPBOARD_TEXT}-byte limit"
            );
            return;
        }
        // The client converts its text to the desktop's LF line ends, as
        // GUEST_LINEEND_LF asks, so the text goes on the desktop as it
        // came. But it is compared, and remembered, with LF line ends, as
        // the desktop's text is: a client that sent CRLF anyway would
        // otherwise not recognise its own text coming back, normalised.
        let crossing = lf_line_ends(text.clone());
        if self.last_crossed.as_deref() == Some(&*crossing) {
            debug!(
                session = self.session,
                bytes = text.len(),
                hash = text_hash(&text),
                "the client's clipboard text is the text that last crossed; not set again"
            );
            return;
        }
        info!(
            session = self.session,
            bytes = text.len(),
            "putting the client's clipboard text on the desktop"
        );
        debug!(
            session = self.session,
            hash = text_hash(&text),
            compared = text_hash(&crossing),
            last = self.last_crossed.as_deref().map(text_hash),
            "the client's text, as put and as compared, and the text that last crossed"
        );
        self.last_crossed = Some(crossing);
        if let Err(error) = self.clipboard.set(text) {
            warn!(
                session = self.session,
                error = format!("{error:#}"),
                "could not put the client's clipboard text on the desktop"
            );
        }
    }

    /// Offer the client the desktop's clipboard `text`, with LF line
    /// ends, unless it is the text that last crossed. `None` withdraws an
    /// offer standing.
    fn offer(&mut self, text: Option<Arc<str>>) -> Result<()> {
        let Some(has_selection) = self.has_selection() else {
            // Offered once the client announces its capabilities.
            return Ok(());
        };
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            if self.offered.take().is_some() {
                let release = ClipboardRelease {
                    selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
                };
                self.queue_clipboard(VD_AGENT_CLIPBOARD_RELEASE, &release, has_selection)?;
            }
            return Ok(());
        };
        // Before the echo comparison, so that text that crossed and came
        // back with CRLF line ends, as text through RDP does, is the
        // same text; and before it is offered, since GUEST_LINEEND_LF
        // promises the client LF.
        let text = lf_line_ends(text);
        if text.len() > MAX_CLIPBOARD_TEXT {
            warn!(
                session = self.session,
                bytes = text.len(),
                "not offering the desktop's clipboard text to the client: it is over the \
                 {MAX_CLIPBOARD_TEXT}-byte limit"
            );
            return Ok(());
        }
        if self.last_crossed.as_deref() == Some(&*text) {
            debug!(
                session = self.session,
                bytes = text.len(),
                hash = text_hash(&text),
                "the desktop's clipboard text is the text that last crossed; not offered again"
            );
            return Ok(());
        }
        let by_demand = self
            .client_caps
            .as_ref()
            .is_some_and(|caps| caps.has_capability(VD_AGENT_CAP_CLIPBOARD_BY_DEMAND));
        if !by_demand {
            debug!(
                session = self.session,
                "the client does not take clipboard grabs; the desktop's text is not offered"
            );
            return Ok(());
        }
        info!(
            session = self.session,
            bytes = text.len(),
            "offering the desktop's clipboard text to the client"
        );
        debug!(
            session = self.session,
            hash = text_hash(&text),
            last = self.last_crossed.as_deref().map(text_hash),
            "the desktop's text, as offered, and the text that last crossed"
        );
        let grab = ClipboardGrab {
            selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            types: vec![VD_AGENT_CLIPBOARD_UTF8_TEXT],
        };
        // Recorded only once the grab is on its way: the client cannot ask
        // for text it was never offered, and text not offered has not
        // crossed.
        if self.queue_clipboard(VD_AGENT_CLIPBOARD_GRAB, &grab, has_selection)? {
            self.last_crossed = Some(text.clone());
            self.offered = Some(text);
        }
        Ok(())
    }
}

/// `text` with each CRLF made LF.
///
/// Andris announces `GUEST_LINEEND_LF`, which tells the client that the
/// desktop's text ends its lines in LF, and the client converts to and
/// from its own line ends on that understanding. The desktop's text need
/// not keep that promise: RDP carries text with CRLF line ends, and
/// xrdp's clipboard bridge puts it on the desktop as it came. Only CRLF
/// is converted. A lone CR is not a line end on any platform a client
/// runs on today, so one in the text is meant, and is left alone.
fn lf_line_ends(text: Arc<str>) -> Arc<str> {
    if text.contains("\r\n") {
        Arc::from(text.replace("\r\n", "\n"))
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use shakenfist_spice_protocol::messages::vd_agent::MonConfig;

    use super::*;

    fn header(chunk: &[u8]) -> VdAgentMessageHeader {
        VdAgentMessageHeader::decode(chunk).unwrap()
    }

    #[test]
    fn a_small_message_is_one_chunk_led_by_its_header() {
        let chunks = chunk_message(VD_AGENT_REPLY, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), VdAgentMessageHeader::SIZE + 8);
        assert_eq!(
            header(&chunks[0]),
            VdAgentMessageHeader::new(VD_AGENT_REPLY, 8)
        );
    }

    #[test]
    fn the_header_counts_against_the_first_chunk() {
        let fits = MAX_CHUNK - VdAgentMessageHeader::SIZE;
        for (body, expected) in [
            (fits, vec![MAX_CHUNK]),
            (fits + 1, vec![MAX_CHUNK, 1]),
            (fits + 2 * MAX_CHUNK, vec![MAX_CHUNK; 3]),
            (5000, vec![MAX_CHUNK, MAX_CHUNK, 5020 - 2 * MAX_CHUNK]),
        ] {
            let payload: Vec<u8> = (0..body).map(|i| i as u8).collect();
            let chunks = chunk_message(VD_AGENT_CLIPBOARD, &payload).unwrap();
            let sizes: Vec<usize> = chunks.iter().map(Vec::len).collect();
            assert_eq!(sizes, expected, "a {body}-byte body");
            let mut reassembler = AgentReassembler::default();
            let items: Vec<_> = chunks.iter().flat_map(|c| reassembler.push(c)).collect();
            assert_eq!(
                items,
                vec![AgentStreamItem::Message(AgentMessage {
                    header: VdAgentMessageHeader::new(VD_AGENT_CLIPBOARD, body as u32),
                    payload,
                })]
            );
        }
    }

    #[test]
    fn the_client_window_closes_after_ten_and_reopens_by_batches() {
        let mut window = ClientWindow::new();
        for _ in 0..4 {
            window.take().unwrap();
            window.consumed();
        }
        assert!(!window.owes(), "four is short of a batch");
        window.take().unwrap();
        window.consumed();
        assert!(window.owes());
        for _ in 0..5 {
            window.take().unwrap();
        }
        assert!(window.take().is_err(), "an eleventh chunk has no token");
        assert_eq!(window.hand_back(), 5);
        assert!(!window.owes());
        assert_eq!(window.left, 5);
        window.take().unwrap();
    }

    #[test]
    fn outgoing_spends_one_token_a_chunk_and_saturates() {
        let mut outgoing = Outgoing::default();
        assert!(outgoing.push(VD_AGENT_CLIPBOARD, &[0; 3000]).unwrap());
        assert_eq!(outgoing.chunks.len(), 2);
        assert!(outgoing.next().is_none(), "no credit");
        outgoing.credit = 1;
        assert_eq!(outgoing.next().map(|c| c.len()), Some(MAX_CHUNK));
        assert!(outgoing.next().is_none());
        outgoing.credit = u32::MAX;
        outgoing.credit = outgoing.credit.saturating_add(5);
        assert_eq!(outgoing.credit, u32::MAX);
        assert_eq!(outgoing.next().map(|c| c.len()), Some(3020 - MAX_CHUNK));
        assert_eq!(outgoing.queued, 0);
    }

    #[test]
    fn outgoing_drops_what_would_overfill_it() {
        let mut outgoing = Outgoing::default();
        let big = vec![0; MAX_AGENT_MESSAGE];
        assert!(outgoing.push(VD_AGENT_CLIPBOARD, &big).unwrap());
        assert!(outgoing.has_room(MAX_AGENT_MESSAGE));
        assert!(!outgoing.has_room(MAX_AGENT_MESSAGE + 1));
        assert!(outgoing.push(VD_AGENT_CLIPBOARD, &big).unwrap());
        assert!(!outgoing.has_room(0));
        assert!(!outgoing.push(VD_AGENT_REPLY, &[0; 8]).unwrap());
        assert_eq!(outgoing.queued, MAX_QUEUED);
    }

    fn config(monitors: &[(u32, u32)]) -> MonitorsConfig {
        MonitorsConfig {
            flags: 0,
            monitors: monitors
                .iter()
                .map(|&(width, height)| MonConfig {
                    height,
                    width,
                    depth: 32,
                    x: 0,
                    y: 0,
                })
                .collect(),
            physical_sizes: Vec::new(),
        }
    }

    #[test]
    fn the_first_monitor_sets_the_size() {
        assert_eq!(
            requested_size(&config(&[(1024, 768), (800, 600)])).unwrap(),
            Size {
                width: 1024,
                height: 768
            }
        );
        assert!(requested_size(&config(&[])).is_err());
        assert!(requested_size(&config(&[(0, 768)])).is_err());
        assert!(requested_size(&config(&[(1024, 0)])).is_err());
        assert!(
            requested_size(&config(&[(65536, 65536)])).is_err(),
            "overflows"
        );
        assert!(
            requested_size(&config(&[(32768, 32768)])).is_err(),
            "too many bytes"
        );
        assert!(
            requested_size(&config(&[(100_000, 1)])).is_err(),
            "too wide"
        );
    }

    #[test]
    fn only_crlf_becomes_lf() {
        for (from, to) in [
            ("one line", "one line"),
            ("a\r\nb\r\n", "a\nb\n"),
            ("a\nb", "a\nb"),
            ("a\rb\r", "a\rb\r"),
            ("a\r\r\nb\n\r", "a\r\nb\n\r"),
        ] {
            assert_eq!(&*lf_line_ends(Arc::from(from)), to, "{from:?}");
        }
    }

    #[test]
    fn andris_announces_what_it_does() {
        let caps = capabilities(1);
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
        let set: u32 = caps.caps.iter().map(|word| word.count_ones()).sum();
        assert_eq!(set, 5, "nothing else");
    }
}
