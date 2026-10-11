//! The capture thread: the desktop's size, read with `GetGeometry`, and
//! its pixels, one image per rectangle asked for.
//!
//! Each rectangle is read with MIT-SHM's `ShmGetImage` where that works
//! (see [`super::shm`]), and with `GetImage` otherwise. Which is used is
//! decided once, when the thread's [`Capturer`] is built, and logged at
//! info. MIT-SHM can later give way to `GetImage`, for good and with a
//! warning, in two ways: the segment needs to grow and the server refuses
//! the larger one, or MIT-SHM captures keep failing. Another client can
//! `ShmDetach` andris's segment, say, after which every `ShmGetImage`
//! fails with `BadShmSeg`. After [`SHM_FAILURE_LIMIT`] failed MIT-SHM
//! captures in a row the thread attaches a fresh segment, once; if that
//! fails, or the limit is reached again, it switches to `GetImage`. Either
//! way the capture that reached the limit is retried at once by the new
//! means, so a dead segment costs a few failed captures, not the display.
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

/// How many MIT-SHM captures in a row may fail before the capture thread
/// gives up on its segment: it attaches a fresh one the first time, and
/// switches to `GetImage` the second. Well below the display channel's
/// `MAX_CAPTURE_FAILURES`, so the channel survives both.
const SHM_FAILURE_LIMIT: u32 = 3;

/// What to do about the MIT-SHM segment after a failed capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShmVerdict {
    /// Keep the segment; the failure may be one-off.
    Keep,
    /// Attach a fresh segment in its place, and retry.
    Reattach,
    /// Stop using MIT-SHM, and retry with `GetImage`.
    GiveUp,
}

/// Failed MIT-SHM captures, counted to decide when the segment is dead.
#[derive(Debug, Default)]
struct ShmFailures {
    /// Failed captures since the last success or fresh segment.
    in_a_row: u32,
    /// Whether a fresh segment has already been attached once.
    reattached: bool,
}

impl ShmFailures {
    /// An MIT-SHM capture worked.
    fn succeeded(&mut self) {
        self.in_a_row = 0;
    }

    /// An MIT-SHM capture failed: what to do now. A fresh segment is
    /// tried only once in the thread's life, so a segment that keeps
    /// being taken away ends in `GetImage` rather than a cycle.
    fn failed(&mut self) -> ShmVerdict {
        self.in_a_row += 1;
        if self.in_a_row < SHM_FAILURE_LIMIT {
            ShmVerdict::Keep
        } else if self.reattached {
            ShmVerdict::GiveUp
        } else {
            self.reattached = true;
            self.in_a_row = 0;
            ShmVerdict::Reattach
        }
    }
}

/// The capture connection, and the MIT-SHM segment attached on it if
/// MIT-SHM is in use. Dropping it detaches the segment.
pub(super) struct Capturer {
    conn: RustConnection,
    root: Window,
    shm: Option<Segment>,
    /// Whether `shm` is in use, shared with [`super::X11Platform`].
    using_shm: Arc<AtomicBool>,
    /// Failed MIT-SHM captures, while `shm` is in use.
    shm_failures: ShmFailures,
    /// Whether a capture has found the connection gone. Nothing may
    /// capture after that: [`Segment::capture`] can return `Gone` with
    /// `ShmGetImage` requests still in flight.
    gone: bool,
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
            shm_failures: ShmFailures::default(),
            gone: false,
        })
    }

    /// Capture `rects`, each clipped to the desktop's size now, or the
    /// whole desktop for `None`. Must not be called again once it has
    /// failed with [`Fault::Gone`]; [`serve_capture`] stops instead.
    fn capture(&mut self, rects: Option<&[Rectangle]>) -> Result<Capture, Fault> {
        debug_assert!(
            !self.gone,
            "a capture after the connection has gone could read a segment the server is still \
             writing"
        );
        let captured = self.capture_now(rects);
        if let Err(Fault::Gone(_)) = captured {
            self.gone = true;
        }
        captured
    }

    /// [`Capturer::capture`], without the check that the connection has
    /// not gone.
    fn capture_now(&mut self, rects: Option<&[Rectangle]>) -> Result<Capture, Fault> {
        let size = root_size(&self.conn, self.root)?;
        let rects: Vec<Rectangle> = match rects {
            None => vec![Rectangle::full(size)],
            Some(rects) => rects.iter().filter_map(|rect| rect.clip(size)).collect(),
        };
        let frames = if rects.is_empty() {
            Vec::new()
        } else {
            self.fit(size)?;
            self.read(size, &rects)?
        };
        let patches = rects
            .into_iter()
            .zip(frames)
            .map(|(rect, frame)| Patch::new(rect, frame))
            .collect::<Result<_>>()
            .map_err(Fault::Failed)?;
        Capture::new(size, patches).map_err(Fault::Failed)
    }

    /// Capture each of `rects`, which lie within a desktop of `size`: with
    /// MIT-SHM while it is in use, counting its failures, and with
    /// `GetImage` otherwise. A failure that reaches [`SHM_FAILURE_LIMIT`]
    /// replaces the segment or gives MIT-SHM up, and the capture is tried
    /// again by the new means. That happens at most twice, since a fresh
    /// segment is attached only once and giving up is for good.
    fn read(&mut self, size: Size, rects: &[Rectangle]) -> Result<Vec<Frame>, Fault> {
        loop {
            let Some(segment) = &self.shm else {
                return get_images(&self.conn, self.root, rects);
            };
            let failure = match segment.capture(&self.conn, self.root, rects) {
                Ok(frames) => {
                    self.shm_failures.succeeded();
                    return Ok(frames);
                }
                Err(Fault::Gone(e)) => return Err(Fault::Gone(e)),
                Err(Fault::Failed(e)) => e,
            };
            match self.shm_failures.failed() {
                ShmVerdict::Keep => return Err(Fault::Failed(failure)),
                ShmVerdict::Reattach => self.reattach(size, &failure)?,
                ShmVerdict::GiveUp => self.give_up_shm(&failure),
            }
        }
    }

    /// Replace the MIT-SHM segment, after `failure` and those before it,
    /// with a fresh one for a desktop of `size`. If that cannot be
    /// attached, give MIT-SHM up.
    fn reattach(&mut self, size: Size, failure: &anyhow::Error) -> Result<(), Fault> {
        // Detach the old segment first, so that the two are never
        // attached at once. If another client detached it already, the
        // server refuses this, unchecked and harmlessly.
        if let Some(old) = self.shm.take() {
            old.detach(&self.conn);
        }
        match attach(&self.conn, size) {
            Ok(segment) => {
                warn!(
                    error = format!("{failure:#}"),
                    "{SHM_FAILURE_LIMIT} MIT-SHM captures in a row failed; attached a fresh \
                     segment"
                );
                self.shm = Some(segment);
                Ok(())
            }
            Err(Fault::Gone(e)) => Err(Fault::Gone(e)),
            Err(Fault::Failed(e)) => {
                self.give_up_shm(&e.context(format!(
                    "attaching a fresh segment, after {SHM_FAILURE_LIMIT} failed MIT-SHM \
                     captures in a row"
                )));
                Ok(())
            }
        }
    }

    /// Stop using MIT-SHM for good, because of `why`.
    fn give_up_shm(&mut self, why: &anyhow::Error) {
        if let Some(old) = self.shm.take() {
            old.detach(&self.conn);
        }
        self.using_shm.store(false, Ordering::Relaxed);
        warn!(
            error = format!("{why:#}"),
            "MIT-SHM capture keeps failing; capturing with GetImage from now on"
        );
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
            // Stopping here is what makes Segment::capture's early return
            // on Gone sound: it may leave ShmGetImage requests in flight,
            // and this drops the Capturer, and so the segment, without
            // reading it again. Capturer::capture asserts as much.
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
    fn repeated_shm_failures_reattach_once_then_give_up() {
        let mut failures = ShmFailures::default();
        for _ in 1..SHM_FAILURE_LIMIT {
            assert_eq!(failures.failed(), ShmVerdict::Keep);
        }
        assert_eq!(failures.failed(), ShmVerdict::Reattach);
        // The fresh segment starts a new count.
        for _ in 1..SHM_FAILURE_LIMIT {
            assert_eq!(failures.failed(), ShmVerdict::Keep);
        }
        assert_eq!(failures.failed(), ShmVerdict::GiveUp);
    }

    #[test]
    fn a_success_resets_the_count_but_not_the_reattach() {
        let mut failures = ShmFailures::default();
        for _ in 0..10 {
            for _ in 1..SHM_FAILURE_LIMIT {
                assert_eq!(failures.failed(), ShmVerdict::Keep);
            }
            failures.succeeded();
        }
        for _ in 1..SHM_FAILURE_LIMIT {
            failures.failed();
        }
        assert_eq!(failures.failed(), ShmVerdict::Reattach);
        failures.succeeded();
        for _ in 1..SHM_FAILURE_LIMIT {
            assert_eq!(failures.failed(), ShmVerdict::Keep);
        }
        assert_eq!(
            failures.failed(),
            ShmVerdict::GiveUp,
            "a segment is replaced only once"
        );
    }

    #[test]
    fn shm_gives_up_well_before_the_display_does() {
        // Two rounds of failures, the second ending in a retry with
        // GetImage, must not close the display channel on the way.
        const {
            assert!(2 * (SHM_FAILURE_LIMIT - 1) < crate::server::display::MAX_CAPTURE_FAILURES);
        }
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
