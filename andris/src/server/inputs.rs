//! The inputs channel.
//!
//! For now it reads and ignores what the client sends; it sends nothing.

use anyhow::Result;

use super::channel::{ChannelConn, ChannelContext};
use super::opcodes::InputsOpcode;

/// Serve an inputs channel until the client closes it.
pub async fn run(_ctx: ChannelContext, mut conn: ChannelConn<InputsOpcode>) -> Result<()> {
    while let Some(message) = conn.recv().await? {
        conn.ignore(&message);
    }
    Ok(())
}
