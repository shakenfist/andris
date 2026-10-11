//! The capture thread: the desktop's size and pixels, read with
//! `GetGeometry` and `GetImage` on the root window.

use std::sync::mpsc::{Receiver, SyncSender};

use anyhow::Result;
use tracing::debug;
use x11rb::protocol::xproto::{self, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

use super::{Fault, answer, log_lost};
use crate::platform::{Frame, Size};

/// A request to the capture thread, with where to send its answer.
pub(super) enum CaptureRequest {
    Size(SyncSender<Result<Size>>),
    Capture(SyncSender<Result<Frame>>),
}

/// Serve capture requests until the queue closes or the connection fails.
pub(super) fn serve_capture(
    conn: RustConnection,
    root: Window,
    requests: Receiver<CaptureRequest>,
) {
    while let Ok(request) = requests.recv() {
        let gone = match request {
            CaptureRequest::Size(reply) => answer(reply, root_size(&conn, root).map(|s| s.0)),
            CaptureRequest::Capture(reply) => answer(reply, capture(&conn, root)),
        };
        if let Some(e) = gone {
            log_lost("capture", "capture has stopped", &e);
            return;
        }
    }
    debug!("the X11 capture thread is stopping");
}

/// The root window's size now, as a checked [`Size`] and as X's `u16`s.
fn root_size(conn: &RustConnection, root: Window) -> Result<(Size, u16, u16), Fault> {
    let geometry = xproto::get_geometry(conn, root)?.reply()?;
    let size = Size {
        width: geometry.width.into(),
        height: geometry.height.into(),
    };
    size.check().map_err(Fault::Failed)?;
    Ok((size, geometry.width, geometry.height))
}

/// Capture the root window at its current size.
///
/// The checks at startup ensured a `ZPixmap` image here is 32 bits a
/// pixel, LSBFirst, with red, green and blue masks that put the bytes
/// in memory as B, G, R and one more. At depth 24 X does not promise
/// that fourth byte is zero, but [`Frame`] ignores it. A 32 bit pixel
/// fills a scanline unit whatever the scanline pad, so rows are
/// `width * 4` bytes apart.
fn capture(conn: &RustConnection, root: Window) -> Result<Frame, Fault> {
    let (_, width, height) = root_size(conn, root)?;
    let image =
        xproto::get_image(conn, ImageFormat::Z_PIXMAP, root, 0, 0, width, height, !0)?.reply()?;
    Frame::new(
        width.into(),
        height.into(),
        usize::from(width) * 4,
        image.data,
    )
    .map_err(|e| Fault::Failed(e.context("the X server's image is not the size it said")))
}
