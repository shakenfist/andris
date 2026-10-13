//! The damage thread: follows XDamage on the root window, and the root
//! window's size, and reports both to the platform's [`DamageHub`].
//!
//! The damage object reports `RawRectangles`: one event per rectangle
//! drawn, with nothing accumulated in the server, so there is nothing
//! to subtract. Every event already queued is reported to the hub in
//! one call. A root window that changes size, seen as a
//! `ConfigureNotify`, makes every subscription's pending damage the whole
//! desktop at the new size instead.

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, error, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self as xdamage, ReportLevel};
use x11rb::protocol::xproto::{self, ChangeWindowAttributesAux, EventMask, Window};
use x11rb::rust_connection::RustConnection;

use super::{create_wake_window, is_wake, lost, x_error};
use crate::platform::{DamageHub, GoneSender, Rectangle, Size};

/// The DAMAGE version andris asks for. Andris uses only what 1.0 has:
/// `Create`, and the `Notify` event.
const DAMAGE_VERSION: (u32, u32) = (1, 1);

/// The most events reported to the hub in one call. Under a stream of
/// damage the queue may never empty, and the hub must still hear of it.
const BATCH_LIMIT: usize = 1024;

/// Set up the damage connection: make the DAMAGE handshake, create the
/// window the drop wakes the thread through, create a `RawRectangles`
/// damage object on `root`, and ask for the root window's size changes.
/// Returns the wake window, the damage object, and the root window's
/// size once its changes are being followed.
pub(super) fn start_damage(
    conn: &RustConnection,
    root: Window,
) -> Result<(Window, xdamage::Damage, Size)> {
    // DAMAGE requires QueryVersion before any other request.
    xdamage::query_version(conn, DAMAGE_VERSION.0, DAMAGE_VERSION.1)?
        .reply()
        .context("the DAMAGE QueryVersion handshake")?;
    let wake_window = create_wake_window(conn, root)?;
    let damage = conn.generate_id()?;
    xdamage::create(conn, damage, root, ReportLevel::RAW_RECTANGLES)?
        .check()
        .context("creating a damage object on the root window")?;
    xproto::change_window_attributes(
        conn,
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
    )?
    .check()
    .context("asking for the root window's size changes")?;
    // Read after the request above, so a resize is either seen here or
    // reported as an event.
    let geometry = xproto::get_geometry(conn, root)?
        .reply()
        .context("reading the root window's size")?;
    let size = Size {
        width: geometry.width.into(),
        height: geometry.height.into(),
    };
    Ok((wake_window, damage, size))
}

/// The pixels of an X rectangle on the root window that have
/// non-negative coordinates, or `None` if there are none.
pub(super) fn to_rectangle(area: &xproto::Rectangle) -> Option<Rectangle> {
    let left = i32::from(area.x).max(0);
    let top = i32::from(area.y).max(0);
    let right = i32::from(area.x) + i32::from(area.width);
    let bottom = i32::from(area.y) + i32::from(area.height);
    if right <= left || bottom <= top {
        return None;
    }
    // Each value is now between 0 and 2^16 + 2^15, so the casts are exact.
    Some(Rectangle::new(
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

/// What one batch of events amounts to.
#[derive(Debug, Default, PartialEq, Eq)]
struct Batch {
    /// The damaged rectangles, in the order reported.
    damaged: Vec<Rectangle>,
    /// The root window's new size, if it changed.
    resized: Option<Size>,
    /// Whether the platform's drop woke the thread to stop.
    stop: bool,
}

/// Report a batch to `hub`: a resize, which makes the whole desktop
/// damaged, or else the damaged rectangles.
fn report(hub: &DamageHub, batch: &Batch) -> Result<()> {
    match batch.resized {
        Some(size) => hub.resize(size),
        None => hub.damage(&batch.damaged),
    }
}

/// The state [`serve_damage`] needs to read events.
pub(super) struct DamageThread {
    /// The damage connection, shared with the platform to wake the
    /// thread.
    pub conn: Arc<RustConnection>,
    /// The root window, whose resizes are followed.
    pub root: Window,
    /// The window the platform's drop wakes the thread through.
    pub wake_window: Window,
    /// The damage object on the root window.
    pub damage: xdamage::Damage,
    /// The root window's size as last reported to the hub.
    pub size: Size,
    /// Where damage and resizes are reported.
    pub hub: Arc<DamageHub>,
    /// Set when the thread stops for any reason but the platform's drop.
    pub gone: GoneSender,
}

impl DamageThread {
    /// Add `event` to `batch`. Returns an X error the server sent, for
    /// the caller to log.
    fn add(&mut self, event: Event, batch: &mut Batch) -> Option<anyhow::Error> {
        match event {
            Event::DamageNotify(notify) if notify.damage == self.damage => {
                batch.damaged.extend(to_rectangle(&notify.area));
            }
            Event::ConfigureNotify(configure) if configure.window == self.root => {
                let size = Size {
                    width: configure.width.into(),
                    height: configure.height.into(),
                };
                if size != self.size {
                    self.size = size;
                    batch.resized = Some(size);
                }
            }
            Event::ClientMessage(message) if message.window == self.wake_window => {
                if is_wake(&message) {
                    batch.stop = true;
                } else {
                    debug!("ignored a ClientMessage to the damage thread's wake window");
                }
            }
            Event::Error(e) => return Some(x_error(&e)),
            _ => {}
        }
        None
    }
}

/// Follow damage and resizes until woken to stop or something fails.
/// Unless it was woken, the platform's gone signal is set and the hub is
/// made [`gone`] as the thread stops, so every subscription learns the
/// desktop has gone rather than waiting for damage that will never come.
///
/// [`gone`]: DamageHub::gone
pub(super) fn serve_damage(mut thread: DamageThread) {
    let Some(why) = follow(&mut thread) else {
        debug!("the X11 damage thread is stopping");
        return;
    };
    thread.gone.set(why.clone());
    if let Err(e) = thread.hub.gone(why) {
        error!(
            error = format!("{e:#}"),
            "the X11 damage thread cannot report that the X display has gone"
        );
    }
}

/// Follow damage and resizes. Returns `None` once woken to stop, and
/// otherwise why the thread cannot go on, having logged it.
fn follow(thread: &mut DamageThread) -> Option<String> {
    let mut warned = false;
    loop {
        let mut batch = Batch::default();
        let mut next = thread.conn.wait_for_event().map(Some);
        let mut events = 0;
        let failure = loop {
            match next {
                Ok(Some(event)) => {
                    events += 1;
                    if let Some(e) = thread.add(event, &mut batch)
                        && !warned
                    {
                        warned = true;
                        warn!(
                            error = format!("{e:#}"),
                            "the X server reported an error to the damage thread; later errors \
                             are not logged"
                        );
                    }
                    if events == BATCH_LIMIT || batch.stop {
                        break None;
                    }
                    next = thread.conn.poll_for_event();
                }
                Ok(None) => break None,
                Err(e) => break Some(e),
            }
        };
        if let Err(e) = report(&thread.hub, &batch) {
            error!(
                error = format!("{e:#}"),
                "the X11 damage thread cannot report damage, and is stopping"
            );
            return Some(format!("the X11 damage thread cannot report damage: {e:#}"));
        }
        if let Some(e) = failure {
            return Some(lost(
                &thread.gone,
                "damage",
                "damage is no longer followed",
                &e,
            ));
        }
        if batch.stop {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x_rect(x: i16, y: i16, width: u16, height: u16) -> xproto::Rectangle {
        xproto::Rectangle {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn to_rectangle_keeps_what_lies_right_of_and_below_the_origin() {
        assert_eq!(
            to_rectangle(&x_rect(10, 20, 30, 40)),
            Some(Rectangle::new(10, 20, 30, 40))
        );
        assert_eq!(
            to_rectangle(&x_rect(-5, -10, 30, 40)),
            Some(Rectangle::new(0, 0, 25, 30))
        );
        assert_eq!(
            to_rectangle(&x_rect(i16::MAX, i16::MAX, u16::MAX, u16::MAX)),
            Some(Rectangle::new(32767, 32767, 65535, 65535))
        );
        assert_eq!(to_rectangle(&x_rect(-30, 0, 30, 10)), None);
        assert_eq!(to_rectangle(&x_rect(0, -1, 10, 1)), None);
        assert_eq!(to_rectangle(&x_rect(0, 0, 0, 10)), None);
    }

    #[test]
    fn a_resize_reports_the_whole_new_desktop() {
        let size = Size {
            width: 256,
            height: 128,
        };
        let hub = DamageHub::new(size);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();

        let batch = Batch {
            damaged: vec![Rectangle::new(0, 0, 1, 1)],
            ..Batch::default()
        };
        report(&hub, &batch).unwrap();
        assert_eq!(subscription.take().unwrap().tile_count(), 1);

        let larger = Size {
            width: 512,
            height: 256,
        };
        let batch = Batch {
            damaged: vec![Rectangle::new(0, 0, 1, 1)],
            resized: Some(larger),
            stop: false,
        };
        report(&hub, &batch).unwrap();
        let taken = subscription.take().unwrap();
        assert_eq!(taken.size(), larger);
        assert_eq!(taken.tile_count(), 8 * 4);
    }
}
