//! The main channel.
//!
//! For now it accepts the channel and reads and ignores what the client
//! sends; it sends nothing.

use anyhow::Result;

use super::channel::{ChannelConn, ChannelContext};
use super::opcodes::MainOpcode;

/// Serve a main channel until the client closes it.
pub async fn run(_ctx: ChannelContext, mut conn: ChannelConn<MainOpcode>) -> Result<()> {
    while let Some(message) = conn.recv().await? {
        conn.ignore(&message);
    }
    Ok(())
}
