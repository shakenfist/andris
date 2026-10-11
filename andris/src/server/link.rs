//! The link handshake and ticket authentication, as the server sees
//! them.
//!
//! The order follows kerbside's proxy: read the link message, refuse a
//! client or channel andris cannot serve with a link error, send a reply
//! carrying a fresh RSA key, read and check the ticket, and send the
//! verdict. Every read and write has a time limit, because the peer is
//! not yet authenticated.
//!
//! A wrong ticket is answered only after [`HandshakeTimeouts::wrong_ticket`],
//! with the connection's handshake slot still held, so that together with
//! the per-address cap on handshakes in progress a peer can only guess
//! slowly. RSA key generation, the costliest step, is capped separately
//! by a semaphore the caller passes in.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use shakenfist_spice_protocol::link::{
    SpiceLinkReply, generate_ticket_keypair, read_auth_ticket, read_link_mess, send_auth_result,
    send_link_reply,
};
use shakenfist_spice_protocol::reader::LinkError;
use shakenfist_spice_protocol::{ChannelType, SpiceError, capabilities};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Semaphore;
use tracing::{debug, info};

use super::LogThrottle;
use super::channel::ClientCaps;
use super::session::{Session, SessionRegistry};
use crate::config::Ticket;

/// The time limits on a connection's handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeTimeouts {
    /// Each first stage, which a peer can stall without having proved
    /// anything: the TLS accept on the TLS listener, then on every
    /// listener reading the link message. Each gets this limit on its own.
    pub first_stage: Duration,
    /// Each later stage: waiting to generate a key, the reply and ticket
    /// together, and the verdict.
    pub stage: Duration,
    /// How long a client whose ticket is wrong waits to be told so.
    pub wrong_ticket: Duration,
}

impl Default for HandshakeTimeouts {
    fn default() -> Self {
        Self {
            first_stage: Duration::from_secs(3),
            stage: Duration::from_secs(10),
            wrong_ticket: Duration::from_secs(1),
        }
    }
}

/// What a handshake needs from the server besides the connection.
pub struct HandshakeContext<'a> {
    pub registry: &'a SessionRegistry,
    pub ticket: &'a Ticket,
    pub timeouts: HandshakeTimeouts,
    /// Permits to generate a ticket key; one is held for each generation.
    pub key_generations: &'a Arc<Semaphore>,
    /// Throttles the log line for a wrong ticket.
    pub wrong_tickets: &'a LogThrottle,
    /// The peer, for the log.
    pub peer: SocketAddr,
}

/// `SPICE_INPUTS_CAP_KEY_SCANCODE`, bit 0 of the inputs channel's
/// capabilities (spice-protocol's `spice/enums.h`). Ryll's crate does not
/// name it yet (ryll#503).
pub const INPUTS_CAP_KEY_SCANCODE: u32 = 1 << 0;

/// The common capabilities andris's link reply offers: ticket
/// authentication with a mechanism selector, and the mini header.
pub const REPLY_COMMON_CAPS: [u32; 1] =
    [capabilities::AUTH_SELECTION | capabilities::AUTH_SPICE | capabilities::MINI_HEADER];

/// The common capabilities a client must advertise. Without
/// `AUTH_SELECTION` the client sends no mechanism selector, and without
/// `MINI_HEADER` it frames messages differently; andris speaks neither
/// alternative.
const REQUIRED_CLIENT_CAPS: u32 = capabilities::AUTH_SELECTION | capabilities::MINI_HEADER;

/// Kerbside's offer for each channel, so a proxy in front of andris sees
/// the capabilities it expects of a backend.
const MAIN_CAPS: [u32; 1] = [capabilities::MAIN_SEMI_SEAMLESS_MIGRATE
    | capabilities::MAIN_NAME_AND_UUID
    | capabilities::MAIN_AGENT_CONNECTED_TOKENS
    | capabilities::MAIN_SEAMLESS_MIGRATE];
const DISPLAY_CAPS: [u32; 1] = [capabilities::DISPLAY_MONITORS_CONFIG
    | capabilities::DISPLAY_STREAM_REPORT
    | capabilities::DISPLAY_PREF_COMPRESSION
    | capabilities::DISPLAY_PREF_VIDEO_CODEC_TYPE];
const INPUTS_CAPS: [u32; 1] = [INPUTS_CAP_KEY_SCANCODE];

/// The channels andris serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServedChannel {
    Main,
    Display,
    Inputs,
    Cursor,
}

impl ServedChannel {
    /// The served channel of this type, if andris serves it.
    pub fn from_type(channel_type: ChannelType) -> Option<Self> {
        match channel_type {
            ChannelType::Main => Some(ServedChannel::Main),
            ChannelType::Display => Some(ServedChannel::Display),
            ChannelType::Inputs => Some(ServedChannel::Inputs),
            ChannelType::Cursor => Some(ServedChannel::Cursor),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ServedChannel::Main => "main",
            ServedChannel::Display => "display",
            ServedChannel::Inputs => "inputs",
            ServedChannel::Cursor => "cursor",
        }
    }

    /// The channel capabilities andris's link reply offers.
    pub fn reply_caps(self) -> &'static [u32] {
        match self {
            ServedChannel::Main => &MAIN_CAPS,
            ServedChannel::Display => &DISPLAY_CAPS,
            ServedChannel::Inputs => &INPUTS_CAPS,
            ServedChannel::Cursor => &[],
        }
    }
}

/// An authenticated channel, ready to serve.
#[derive(Debug)]
pub struct Linked {
    pub channel: ServedChannel,
    pub channel_id: u8,
    pub caps: ClientCaps,
    pub session: Arc<Session>,
}

/// Whether the password a client sent is the ticket.
///
/// A simple xor-fold over every byte of the ticket: the time it takes
/// depends on the ticket's length, never on where the two first differ.
/// That is all a ticket comparison needs to be constant-time against.
fn ticket_matches(password: &[u8], ticket: &[u8]) -> bool {
    let mut diff = u8::from(password.len() != ticket.len());
    for (i, want) in ticket.iter().enumerate() {
        diff |= want ^ password.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// Whether a `read_auth_ticket` failure means the ticket blob was wrong,
/// rather than that the connection failed.
fn is_bad_ticket(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<LinkError>(),
        Some(LinkError::DecryptFailed | LinkError::BadUtf8)
    )
}

/// Run `fut` under `limit`, naming `what` on a timeout.
pub async fn timed<T>(
    limit: Duration,
    what: &str,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(limit, fut)
        .await
        .with_context(|| format!("{what} timed out"))?
}

/// Send a link reply carrying only `error`.
async fn refuse<S>(stream: &mut S, limit: Duration, error: SpiceError) -> Result<Option<Linked>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    timed(
        limit,
        "sending a link refusal",
        send_link_reply(stream, &SpiceLinkReply::error_reply(error)),
    )
    .await?;
    Ok(None)
}

/// Link and authenticate one connection, reading its link message within
/// the first-stage limit.
///
/// Returns `Ok(None)` when the client was refused and told why, and an
/// error when the connection failed or stalled. A main channel that
/// authenticates begins a new session, replacing the current one.
///
/// Refusals before the ticket is checked log at debug level only: an
/// unauthenticated peer can provoke them as fast as it can connect.
pub async fn handshake<S>(stream: &mut S, ctx: &HandshakeContext<'_>) -> Result<Option<Linked>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let stage = ctx.timeouts.stage;
    let registry = ctx.registry;
    let link = timed(
        ctx.timeouts.first_stage,
        "reading the link message",
        read_link_mess(stream),
    )
    .await?;

    let common = link.common_caps.first().copied().unwrap_or(0);
    if common & REQUIRED_CLIENT_CAPS != REQUIRED_CLIENT_CAPS {
        debug!(
            peer = %ctx.peer,
            common_caps = ?link.common_caps,
            "refusing a client without AUTH_SELECTION and MINI_HEADER"
        );
        return refuse(stream, stage, SpiceError::VersionMismatch).await;
    }

    let Some(channel) = ChannelType::from_u8(link.channel_type).and_then(ServedChannel::from_type)
    else {
        debug!(
            channel_type = link.channel_type,
            "refusing a channel andris does not serve"
        );
        return refuse(stream, stage, SpiceError::ChannelUnavailable).await;
    };
    if link.channel_id != 0 {
        debug!(
            channel = channel.name(),
            channel_id = link.channel_id,
            "refusing a channel id other than 0"
        );
        return refuse(stream, stage, SpiceError::ChannelUnavailable).await;
    }

    // A secondary channel must name the current session.
    let existing = match channel {
        ServedChannel::Main => None,
        _ => match registry.lookup(link.connection_id) {
            Some(session) => Some(session),
            None => {
                debug!(
                    channel = channel.name(),
                    connection_id = link.connection_id,
                    "refusing a channel for a session that is not current"
                );
                return refuse(stream, stage, SpiceError::BadConnectionId).await;
            }
        },
    };

    // RSA key generation takes long enough to stall a runtime thread. A
    // fresh key for each connection, never reused (RUSTSEC-2023-0071).
    // The permit moves into the blocking task, so it is held for as long
    // as the generation runs, even if this handshake is dropped meanwhile.
    let permit = timed(stage, "waiting to generate a ticket key", async {
        ctx.key_generations
            .clone()
            .acquire_owned()
            .await
            .context("the key generation semaphore is closed")
    })
    .await?;
    let (key, public_der) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        generate_ticket_keypair()
    })
    .await
    .context("the key generation task failed")?
    .context("generating the ticket key")?;

    let reply = SpiceLinkReply {
        error: SpiceError::Ok,
        pub_key: public_der,
        common_caps: REPLY_COMMON_CAPS.to_vec(),
        channel_caps: channel.reply_caps().to_vec(),
    };
    let password = timed(stage, "the link reply and ticket", async {
        send_link_reply(stream, &reply).await?;
        match read_auth_ticket(stream, &key).await {
            Ok(password) => Ok(Some(password)),
            Err(e) if is_bad_ticket(&e) => Ok(None),
            Err(e) => Err(e),
        }
    })
    .await?;

    let accepted =
        password.is_some_and(|password| ticket_matches(password.as_bytes(), ctx.ticket.as_bytes()));
    if !accepted {
        if let Some(refused) = ctx.wrong_tickets.should_log() {
            info!(
                peer = %ctx.peer,
                channel = channel.name(),
                refused,
                "refusing a client whose ticket is wrong (logged at most every {}s)",
                LogThrottle::INTERVAL.as_secs()
            );
        }
        // The caller's handshake slot stays held through this wait.
        tokio::time::sleep(ctx.timeouts.wrong_ticket).await;
        timed(
            stage,
            "sending the auth result",
            send_auth_result(stream, SpiceError::PermissionDenied),
        )
        .await?;
        return Ok(None);
    }

    // The session is in place before the client hears it is in, so a
    // client never links a secondary channel to a session not yet begun.
    let session = match existing {
        None => registry.begin()?,
        Some(session) => {
            if session.is_cancelled() {
                debug!(
                    channel = channel.name(),
                    session = session.id(),
                    "the session ended during the handshake"
                );
                return Ok(None);
            }
            session
        }
    };
    let sent = timed(
        stage,
        "sending the auth result",
        send_auth_result(stream, SpiceError::Ok),
    )
    .await;
    if let Err(e) = sent {
        if channel == ServedChannel::Main {
            registry.end(&session);
        }
        return Err(e);
    }

    Ok(Some(Linked {
        channel,
        channel_id: link.channel_id,
        caps: ClientCaps {
            common: link.common_caps,
            channel: link.channel_caps,
        },
        session,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_comparison() {
        assert!(ticket_matches(b"secret", b"secret"));
        assert!(!ticket_matches(b"secreT", b"secret"));
        assert!(!ticket_matches(b"secre", b"secret"));
        assert!(!ticket_matches(b"secret!", b"secret"));
        assert!(!ticket_matches(b"", b"secret"));
        assert!(!ticket_matches(b"secret\0", b"secret"));
    }

    #[test]
    fn reply_caps_match_kerbsides_offer() {
        assert_eq!(REPLY_COMMON_CAPS, [0b1011]);
        assert_eq!(ServedChannel::Main.reply_caps(), &[0xf]);
        assert_eq!(
            ServedChannel::Display.reply_caps(),
            &[(1 << 1) | (1 << 4) | (1 << 6) | (1 << 12)]
        );
        assert_eq!(ServedChannel::Inputs.reply_caps(), &[1]);
        assert!(ServedChannel::Cursor.reply_caps().is_empty());
    }

    #[test]
    fn served_channels() {
        for (channel_type, served) in [
            (ChannelType::Main, ServedChannel::Main),
            (ChannelType::Display, ServedChannel::Display),
            (ChannelType::Inputs, ServedChannel::Inputs),
            (ChannelType::Cursor, ServedChannel::Cursor),
        ] {
            assert_eq!(ServedChannel::from_type(channel_type), Some(served));
            assert_eq!(served.name(), channel_type.name());
        }
        assert_eq!(ServedChannel::from_type(ChannelType::Playback), None);
        assert_eq!(ServedChannel::from_type(ChannelType::Usbredir), None);
    }
}
