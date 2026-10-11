//! The link handshake and ticket authentication, as the server sees
//! them.
//!
//! The order follows kerbside's proxy: read the link message, refuse a
//! client or channel andris cannot serve with a link error, send a reply
//! carrying a fresh RSA key, read and check the ticket, and send the
//! verdict. Every read and write has a time limit, because the peer is
//! not yet authenticated.

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
use tracing::{debug, info};

use super::channel::ClientCaps;
use super::session::{Session, SessionRegistry};
use crate::config::Ticket;

/// How long each stage of the handshake may take: the link message, the
/// reply and ticket together, and the verdict.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Run `fut` under [`HANDSHAKE_TIMEOUT`], naming `what` on a timeout.
async fn timed<T>(what: &str, fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, fut)
        .await
        .with_context(|| format!("{what} timed out"))?
}

/// Send a link reply carrying only `error`.
async fn refuse<S>(stream: &mut S, error: SpiceError) -> Result<Option<Linked>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    timed(
        "sending a link refusal",
        send_link_reply(stream, &SpiceLinkReply::error_reply(error)),
    )
    .await?;
    Ok(None)
}

/// Link and authenticate one connection.
///
/// Returns `Ok(None)` when the client was refused and told why, and an
/// error when the connection failed or stalled. A main channel that
/// authenticates begins a new session, replacing the current one.
pub async fn handshake<S>(
    stream: &mut S,
    registry: &SessionRegistry,
    ticket: &Ticket,
) -> Result<Option<Linked>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let link = timed("reading the link message", read_link_mess(stream)).await?;

    let common = link.common_caps.first().copied().unwrap_or(0);
    if common & REQUIRED_CLIENT_CAPS != REQUIRED_CLIENT_CAPS {
        info!(
            common_caps = ?link.common_caps,
            "refusing a client without AUTH_SELECTION and MINI_HEADER"
        );
        return refuse(stream, SpiceError::VersionMismatch).await;
    }

    let Some(channel) = ChannelType::from_u8(link.channel_type).and_then(ServedChannel::from_type)
    else {
        debug!(
            channel_type = link.channel_type,
            "refusing a channel andris does not serve"
        );
        return refuse(stream, SpiceError::ChannelUnavailable).await;
    };
    if link.channel_id != 0 {
        debug!(
            channel = channel.name(),
            channel_id = link.channel_id,
            "refusing a channel id other than 0"
        );
        return refuse(stream, SpiceError::ChannelUnavailable).await;
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
                return refuse(stream, SpiceError::BadConnectionId).await;
            }
        },
    };

    // RSA key generation takes long enough to stall a runtime thread. A
    // fresh key for each connection, never reused (RUSTSEC-2023-0071).
    let (key, public_der) = tokio::task::spawn_blocking(generate_ticket_keypair)
        .await
        .context("the key generation task failed")?
        .context("generating the ticket key")?;

    let reply = SpiceLinkReply {
        error: SpiceError::Ok,
        pub_key: public_der,
        common_caps: REPLY_COMMON_CAPS.to_vec(),
        channel_caps: channel.reply_caps().to_vec(),
    };
    let password = timed("the link reply and ticket", async {
        send_link_reply(stream, &reply).await?;
        match read_auth_ticket(stream, &key).await {
            Ok(password) => Ok(Some(password)),
            Err(e) if is_bad_ticket(&e) => Ok(None),
            Err(e) => Err(e),
        }
    })
    .await?;

    let accepted =
        password.is_some_and(|password| ticket_matches(password.as_bytes(), ticket.as_bytes()));
    if !accepted {
        info!(
            channel = channel.name(),
            "refusing a client whose ticket is wrong"
        );
        timed(
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
