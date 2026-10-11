//! The cursor thread: follows XFixes cursor changes, and publishes each
//! new shape through a `watch` channel.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tracing::{debug, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes;
use x11rb::protocol::xproto::{self, CreateWindowAux, Window, WindowClass};
use x11rb::rust_connection::RustConnection;

use super::{Fault, log_lost, x_error};
use crate::platform::CursorShape;

/// Set up the cursor connection: make the XFixes handshake, create the
/// window the drop wakes the thread through, ask for cursor changes, and
/// read the current shape (`None` if it cannot be used).
pub(super) fn start_cursor(
    conn: &RustConnection,
    root: Window,
) -> Result<(Window, Option<CursorShape>)> {
    xfixes::query_version(conn, 5, 0)?
        .reply()
        .context("the XFIXES QueryVersion handshake")?;
    let wake_window = conn.generate_id()?;
    xproto::create_window(
        conn,
        x11rb::COPY_FROM_PARENT as u8,
        wake_window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new(),
    )?;
    xfixes::select_cursor_input(conn, root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    let shape = match read_cursor(conn) {
        Ok(shape) => Some(shape),
        Err(Fault::Gone(e)) => return Err(e),
        Err(Fault::Failed(e)) => {
            warn!(
                error = format!("{e:#}"),
                "the X cursor cannot be read; starting with none"
            );
            None
        }
    };
    Ok((wake_window, shape))
}

/// The current cursor image.
fn read_cursor(conn: &RustConnection) -> Result<CursorShape, Fault> {
    let image = xfixes::get_cursor_image(conn)?.reply()?;
    // The reply also carries the pointer's position, `image.x` and
    // `image.y`, for when andris sends the cursor's position as well as
    // its shape.
    CursorShape::new(
        image.width,
        image.height,
        image.xhot,
        image.yhot,
        image.cursor_image,
    )
    .map_err(|e| Fault::Failed(e.context("the X server's cursor image is not usable")))
}

/// Follow cursor changes until woken to stop or the connection fails.
/// The thread owns the only sender, so its receivers see it stop.
pub(super) fn serve_cursor(
    conn: Arc<RustConnection>,
    wake_window: Window,
    shapes: watch::Sender<Option<Arc<CursorShape>>>,
) {
    let mut warned = false;
    loop {
        let event = match conn.wait_for_event() {
            Ok(event) => event,
            Err(e) => {
                log_lost("cursor", "cursor changes have stopped", &e);
                return;
            }
        };
        let failure = match event {
            Event::XfixesCursorNotify(_) => match read_cursor(&conn) {
                Ok(shape) => {
                    shapes.send_replace(Some(Arc::new(shape)));
                    None
                }
                Err(Fault::Gone(e)) => {
                    log_lost("cursor", "cursor changes have stopped", &e);
                    return;
                }
                Err(Fault::Failed(e)) => Some(e),
            },
            Event::ClientMessage(message) if message.window == wake_window => {
                debug!("the X11 cursor thread is stopping");
                return;
            }
            Event::Error(e) => Some(x_error(&e)),
            _ => None,
        };
        if let Some(e) = failure
            && !warned
        {
            warned = true;
            warn!(
                error = format!("{e:#}"),
                "reading the X cursor failed; later failures are not logged"
            );
        }
    }
}
