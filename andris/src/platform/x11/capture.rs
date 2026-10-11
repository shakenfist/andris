//! The capture thread: the desktop's size, read with `GetGeometry`, and
//! its pixels, one image per rectangle asked for.
//!
//! Each rectangle is read with MIT-SHM's `ShmGetImage` where that works
//! (see [`super::shm`]), and with `GetImage` otherwise. Which is used is
//! decided once, when the thread's [`Capturer`] is built, and logged at
//! info. Should the segment later need to grow and the server refuse the
//! larger one, the thread falls back to `GetImage` for good, with a
//! warning.
//!
//! The checks at startup ensured a `ZPixmap` image of the root window is
//! 32 bits a pixel, LSBFirst, with red, green and blue masks that put the
//! bytes in memory as B, G, R and one more. At depth 24 X does not promise
//! that fourth byte is zero, but [`Frame`] ignores it. A 32 bit pixel
//! fills a scanline unit whatever the scanline pad, so rows are
//! `width * 4` bytes apart, by either request.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};

use anyhow::{Result, anyhow};
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

use super::shm::{Segment, image_bytes};
use super::{Fault, answer, log_lost};
use crate::platform::{Capture, Frame, Patch, Rectangle, Size};

/// A request to the capture thread, with where to send its answer.
pub(super) enum CaptureRequest {
    /// The desktop's size now.
    Size(SyncSender<Result<Size>>),
    /// Capture these rectangles, or the whole desktop for `None`.
    Capture(Option<Vec<Rectangle>>, SyncSender<Result<Capture>>),
}

/// The capture connection, and the MIT-SHM segment attached on it if
/// MIT-SHM is in use. Dropping it detaches the segment.
pub(super) struct Capturer {
    conn: RustConnection,
    root: Window,
    shm: Option<Segment>,
    /// Whether `shm` is in use, shared with [`super::X11Platform`].
    using_shm: Arc<AtomicBool>,
}

impl Capturer {
    /// Set up capture of `root` on `conn`, a desktop of `size` for now.
    /// MIT-SHM is tried unless `refusal` says why not, and `GetImage` is
    /// used if it is refused or its segment cannot be attached. Logs
    /// which is used, and why, at info. Fails only if the connection
    /// fails.
    pub(super) fn new(
        conn: RustConnection,
        root: Window,
        size: Size,
        refusal: Option<&str>,
        using_shm: Arc<AtomicBool>,
    ) -> Result<Self> {
        let shm = match refusal {
            Some(why) => {
                info!("capturing the screen with GetImage, because {why}");
                None
            }
            None => match attach(&conn, size) {
                Ok(segment) => {
                    info!(
                        bytes = segment.len(),
                        "capturing the screen with MIT-SHM, through a shared memfd"
                    );
                    Some(segment)
                }
                Err(Fault::Gone(e)) => return Err(e),
                Err(Fault::Failed(e)) => {
                    info!(
                        error = format!("{e:#}"),
                        "capturing the screen with GetImage, because attaching an MIT-SHM \
                         segment failed"
                    );
                    None
                }
            },
        };
        using_shm.store(shm.is_some(), Ordering::Relaxed);
        Ok(Self {
            conn,
            root,
            shm,
            using_shm,
        })
    }

    /// Capture `rects`, each clipped to the desktop's size now, or the
    /// whole desktop for `None`.
    fn capture(&mut self, rects: Option<&[Rectangle]>) -> Result<Capture, Fault> {
        let size = root_size(&self.conn, self.root)?;
        let rects: Vec<Rectangle> = match rects {
            None => vec![Rectangle::full(size)],
            Some(rects) => rects.iter().filter_map(|rect| rect.clip(size)).collect(),
        };
        let frames = if rects.is_empty() {
            Vec::new()
        } else {
            self.fit(size)?;
            match &self.shm {
                Some(segment) => segment.capture(&self.conn, self.root, &rects)?,
                None => get_images(&self.conn, self.root, &rects)?,
            }
        };
        let patches = rects
            .into_iter()
            .zip(frames)
            .map(|(rect, frame)| Patch::new(rect, frame))
            .collect::<Result<_>>()
            .map_err(Fault::Failed)?;
        Capture::new(size, patches).map_err(Fault::Failed)
    }

    /// Make the MIT-SHM segment, if one is in use, large enough for a
    /// desktop of `size`. If the server refuses a larger segment, fall
    /// back to `GetImage` for good.
    fn fit(&mut self, size: Size) -> Result<(), Fault> {
        let Some(segment) = &self.shm else {
            return Ok(());
        };
        let needed = screen_bytes(size).map_err(Fault::Failed)?;
        if segment.len() >= needed {
            return Ok(());
        }
        // Detach the old segment first, so that the two are never
        // attached at once.
        if let Some(old) = self.shm.take() {
            old.detach(&self.conn);
        }
        let attached = Segment::attach(&self.conn, needed);
        self.using_shm.store(attached.is_ok(), Ordering::Relaxed);
        match attached {
            Ok(grown) => {
                debug!(
                    bytes = needed,
                    "the desktop grew to {size}; resized the MIT-SHM segment"
                );
                self.shm = Some(grown);
                Ok(())
            }
            Err(Fault::Gone(e)) => Err(Fault::Gone(e)),
            Err(Fault::Failed(e)) => {
                warn!(
                    error = format!("{e:#}"),
                    "the desktop grew to {size} and a larger MIT-SHM segment could not be \
                     attached; capturing with GetImage from now on"
                );
                Ok(())
            }
        }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        if let Some(segment) = self.shm.take() {
            segment.detach(&self.conn);
            // The connection closes as this drops, which detaches the
            // segment anyway; a failure here does not matter.
            let _ = self.conn.flush();
        }
    }
}

/// Attach a segment for a desktop of `size` on `conn`.
fn attach(conn: &RustConnection, size: Size) -> Result<Segment, Fault> {
    Segment::attach(conn, screen_bytes(size).map_err(Fault::Failed)?)
}

/// The bytes of an image of a whole desktop of `size`.
fn screen_bytes(size: Size) -> Result<usize> {
    image_bytes(&Rectangle::full(size))
        .ok_or_else(|| anyhow!("an image of a {size} desktop is too large to address"))
}

/// Serve capture requests until the queue closes or the connection fails.
pub(super) fn serve_capture(mut capturer: Capturer, requests: Receiver<CaptureRequest>) {
    while let Ok(request) = requests.recv() {
        let gone = match request {
            CaptureRequest::Size(reply) => answer(reply, root_size(&capturer.conn, capturer.root)),
            CaptureRequest::Capture(rects, reply) => {
                answer(reply, capturer.capture(rects.as_deref()))
            }
        };
        if let Some(e) = gone {
            log_lost("capture", "capture has stopped", &e);
            return;
        }
    }
    debug!("the X11 capture thread is stopping");
}

/// The root window's size now.
fn root_size(conn: &RustConnection, root: Window) -> Result<Size, Fault> {
    let geometry = xproto::get_geometry(conn, root)?.reply()?;
    let size = Size {
        width: geometry.width.into(),
        height: geometry.height.into(),
    };
    size.check().map_err(Fault::Failed)?;
    Ok(size)
}

/// `rect` as the x, y, width and height of an X request. Fails if it
/// does not fit them; a rectangle within a desktop [`Size::check`]
/// accepts always does.
pub(super) fn x_rect(rect: &Rectangle) -> Result<(i16, i16, u16, u16), Fault> {
    let fits = || -> Option<(i16, i16, u16, u16)> {
        Some((
            i16::try_from(rect.x).ok()?,
            i16::try_from(rect.y).ok()?,
            u16::try_from(rect.width).ok()?,
            u16::try_from(rect.height).ok()?,
        ))
    };
    fits().ok_or_else(|| Fault::Failed(anyhow!("{rect} does not fit an X request")))
}

/// Capture each of `rects`, which must lie within `root`, with
/// `GetImage`. Every request is sent before the first reply is read, so
/// the whole list costs one round trip.
fn get_images(
    conn: &RustConnection,
    root: Window,
    rects: &[Rectangle],
) -> Result<Vec<Frame>, Fault> {
    let requests = rects.iter().map(x_rect).collect::<Result<Vec<_>, _>>()?;
    let cookies = requests
        .into_iter()
        .map(|(x, y, width, height)| {
            xproto::get_image(conn, ImageFormat::Z_PIXMAP, root, x, y, width, height, !0)
        })
        .collect::<Result<Vec<_>, _>>()?;
    rects
        .iter()
        .zip(cookies)
        .map(|(rect, cookie)| {
            let image = cookie.reply()?;
            Frame::new(rect.width, rect.height, rect.width as usize * 4, image.data).map_err(|e| {
                Fault::Failed(e.context("the X server's image is not the size it said"))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x_rect_takes_what_fits_an_x_request() {
        assert_eq!(x_rect(&Rectangle::new(1, 2, 3, 4)).unwrap(), (1, 2, 3, 4));
        assert_eq!(
            x_rect(&Rectangle::new(16383, 16383, 16384, 16384)).unwrap(),
            (16383, 16383, 16384, 16384)
        );
        assert!(x_rect(&Rectangle::new(32768, 0, 1, 1)).is_err());
        assert!(x_rect(&Rectangle::new(0, 0, 65536, 1)).is_err());
    }

    #[test]
    fn screen_bytes_is_four_a_pixel() {
        let size = Size {
            width: 2212,
            height: 1308,
        };
        assert_eq!(screen_bytes(size).unwrap(), 2212 * 1308 * 4);
    }
}
