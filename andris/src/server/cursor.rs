//! The cursor channel.
//!
//! As spice-server does, andris starts the ack window (`SET_ACK`, window
//! [`ACK_WINDOW`]) and then sends `CURSOR_INIT` with the current shape.
//! After that, each shape the platform's [`CursorSource`] reports becomes
//! a `CURSOR_SET`. Shapes may be skipped: when a change arrives while the
//! ack window is full, the shape sent once there is room is the latest.
//!
//! # What a cursor carries
//!
//! A shape is sent as an `ALPHA` cursor: a [`CursorHeader`] with `unique`
//! 0 and no cache flags, then each pixel as a little-endian 32-bit ARGB
//! word, which is how ryll's cursor decoder reads `ALPHA` data. With no
//! shape, the cursor is [`SpiceCursor::none`].
//!
//! `CURSOR_INIT` and `CURSOR_SET` also carry a position and visibility.
//! The position is always (0, 0), spice-server's default, because in
//! client mouse mode the client draws the pointer where its own mouse is,
//! and andris sends no `CURSOR_MOVE`. A message with a shape says the
//! cursor is visible. One that the platform says has no shape says it is
//! not, because a client keeps the shape it had when the cursor it is
//! sent has none.
//!
//! # Size
//!
//! A shape wider or taller than [`MAX_CURSOR_SIDE`] is dropped, with a
//! warning logged once per channel. If the shape is dropped from
//! `CURSOR_INIT`, the cursor sent is none, but still visible.
//!
//! [`CursorSource`]: crate::platform::CursorSource

use std::sync::Arc;

use anyhow::Result;
use shakenfist_spice_protocol::constants::cursor_type;
use shakenfist_spice_protocol::messages::{
    CursorHeader, CursorInit, CursorInitHead, CursorSet, CursorSetHead, SpiceCursor,
};
use tracing::{debug, warn};

use super::channel::{ACK_WINDOW, ChannelConn, ChannelContext};
use super::opcodes::CursorOpcode;
use crate::platform::CursorShape;

/// The largest cursor side andris sends, in pixels.
pub const MAX_CURSOR_SIDE: u16 = 256;

/// Serve a cursor channel until the client closes it.
pub async fn run(ctx: ChannelContext, mut conn: ChannelConn<CursorOpcode>) -> Result<()> {
    let session = ctx.session.id();
    let mut shapes = ctx.platform.cursor.subscribe();
    let mut warned = false;

    conn.start_acks(ACK_WINDOW).await?;
    let current = shapes.borrow_and_update().clone();
    let (cursor, visible) = match current.as_deref() {
        Some(shape) => match spice_cursor(shape, &mut warned) {
            Some(cursor) => (cursor, true),
            None => (SpiceCursor::none(), true),
        },
        None => (SpiceCursor::none(), false),
    };
    let init = CursorInit {
        head: CursorInitHead {
            x: 0,
            y: 0,
            trail_length: 0,
            trail_frequency: 0,
            visible: u8::from(visible),
        },
        cursor,
    };
    conn.wait_for_room().await?;
    conn.send_msg(CursorOpcode::Init, &init).await?;

    // False once the source has gone, after which the channel only
    // reads what the client sends.
    let mut source_open = true;
    loop {
        tokio::select! {
            message = conn.recv() => {
                let Some(message) = message? else {
                    return Ok(());
                };
                conn.ignore(&message);
            }
            changed = shapes.changed(), if source_open => {
                if changed.is_err() {
                    debug!(session, "the cursor source has gone; no more cursor changes");
                    source_open = false;
                    continue;
                }
                // Wait for room first, so that what is sent is the latest
                // shape once there is room.
                conn.wait_for_room().await?;
                let latest: Option<Arc<CursorShape>> = shapes.borrow_and_update().clone();
                let set = match latest.as_deref() {
                    Some(shape) => match spice_cursor(shape, &mut warned) {
                        Some(cursor) => CursorSet {
                            head: CursorSetHead { x: 0, y: 0, visible: 1 },
                            cursor,
                        },
                        None => continue,
                    },
                    None => CursorSet {
                        head: CursorSetHead { x: 0, y: 0, visible: 0 },
                        cursor: SpiceCursor::none(),
                    },
                };
                conn.send_msg(CursorOpcode::Set, &set).await?;
            }
        }
    }
}

/// `shape` as an `ALPHA` cursor, or `None` if it is too large to send,
/// warning about that the first time `warned` is false.
fn spice_cursor(shape: &CursorShape, warned: &mut bool) -> Option<SpiceCursor> {
    if shape.width() > MAX_CURSOR_SIDE || shape.height() > MAX_CURSOR_SIDE {
        if !*warned {
            *warned = true;
            warn!(
                width = shape.width(),
                height = shape.height(),
                "dropping a cursor shape larger than {MAX_CURSOR_SIDE}x{MAX_CURSOR_SIDE} \
                 (logged once per channel)"
            );
        }
        return None;
    }
    let data = shape
        .argb()
        .iter()
        .flat_map(|pixel| pixel.to_le_bytes())
        .collect();
    Some(SpiceCursor {
        flags: 0,
        header: Some(CursorHeader {
            unique_id: 0,
            cursor_type: cursor_type::ALPHA,
            width: shape.width(),
            height: shape.height(),
            hot_spot_x: shape.hot_x(),
            hot_spot_y: shape.hot_y(),
        }),
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels_are_little_endian_argb() {
        let shape = CursorShape::new(2, 1, 1, 0, vec![0x8011_2233, 0xFF00_00FF]).unwrap();
        let mut warned = false;
        let cursor = spice_cursor(&shape, &mut warned).unwrap();
        assert_eq!(cursor.flags, 0);
        assert_eq!(
            cursor.header,
            Some(CursorHeader {
                unique_id: 0,
                cursor_type: cursor_type::ALPHA,
                width: 2,
                height: 1,
                hot_spot_x: 1,
                hot_spot_y: 0,
            })
        );
        // B, G, R, A: what ryll's ALPHA decoder reads.
        assert_eq!(cursor.data, vec![0x33, 0x22, 0x11, 0x80, 0xFF, 0, 0, 0xFF]);
        assert!(!warned);
    }

    #[test]
    fn oversized_shapes_are_dropped_and_warned_once() {
        let mut warned = false;
        let side = MAX_CURSOR_SIDE as usize;
        let largest =
            CursorShape::new(MAX_CURSOR_SIDE, MAX_CURSOR_SIDE, 0, 0, vec![0; side * side]).unwrap();
        assert!(spice_cursor(&largest, &mut warned).is_some());
        let wide = CursorShape::new(MAX_CURSOR_SIDE + 1, 1, 0, 0, vec![0; side + 1]).unwrap();
        assert!(spice_cursor(&wide, &mut warned).is_none());
        assert!(warned);
        let tall = CursorShape::new(1, MAX_CURSOR_SIDE + 1, 0, 0, vec![0; side + 1]).unwrap();
        assert!(spice_cursor(&tall, &mut warned).is_none());
    }
}
