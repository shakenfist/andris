//! The display channel.
//!
//! For now it starts the ack window, then reads and ignores what the
//! client sends.

use anyhow::Result;

use super::channel::{ACK_WINDOW, ChannelConn, ChannelContext};
use super::opcodes::DisplayOpcode;

/// Serve a display channel until the client closes it.
pub async fn run(_ctx: ChannelContext, mut conn: ChannelConn<DisplayOpcode>) -> Result<()> {
    conn.start_acks(ACK_WINDOW).await?;
    while let Some(message) = conn.recv().await? {
        conn.ignore(&message);
    }
    Ok(())
}
