//! The display listener andris registers with qemu: the D-Bus objects
//! qemu calls with each change to the guest's screen.
//!
//! Two interfaces sit at `/org/qemu/Display1/Listener`:
//!
//! - [`Listener`], `org.qemu.Display1.Listener`, whose `Scanout` and
//!   `Update` carry the pixels in the message. qemu uses them when the
//!   screen cannot be shared, as for std-vga, whose screen is VRAM.
//! - [`MapListener`], `org.qemu.Display1.Listener.Unix.Map`, whose
//!   `ScanoutMap` hands over a memfd holding the whole screen, after which
//!   `UpdateMap` carries only rectangles. qemu uses it when the screen is
//!   shareable, as virtio-gpu's 2D resources are, and only for a listener
//!   whose `Interfaces` property names it.
//!
//! Both are served before qemu sees the connection: qemu calls
//! `ScanoutMap` as soon as the listener registers, and a listener that
//! cannot answer then is never offered the map again.
//!
//! Every handler copies what changed into the platform's [`Framebuffer`]
//! and reports the rectangle to the [`DamageHub`]. qemu expects no reply
//! to anything but `ScanoutMap`, which it waits for with its main loop
//! held, so a slow `ScanoutMap` stalls the guest.
//!
//! Each update is logged at debug level with the wall-clock time it
//! arrived, in microseconds since the epoch, and how long the copy took,
//! so that a trace can be lined up with qemu's own trace log, which
//! stamps events with the same clock.

use std::os::fd::AsFd;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use qemu_display::{ScanoutMap, ScanoutMmap};
use serde_bytes::ByteBuf;
use tracing::{debug, warn};
use zbus::fdo;
use zbus::zvariant::Fd;

use super::framebuffer::{Framebuffer, Source};
use crate::platform::{DamageHub, Rectangle, Size};

/// Where qemu expects the listener's objects.
pub const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";

/// The shared-memory interface, as the listener names it to qemu.
pub const MAP_INTERFACE: &str = "org.qemu.Display1.Listener.Unix.Map";

/// What the listener shares with the platform's seams.
pub struct Shared {
    /// The copy of the screen; `None` until qemu sends the first one.
    screen: Mutex<Option<Framebuffer>>,
    /// Signalled when the first screen arrives.
    first_screen: Condvar,
    /// The screen qemu shared, while it shares one. Taken before
    /// `screen` when both are held.
    mapping: Mutex<Option<ScanoutMmap>>,
    /// The last copy that failed, so that a run of the same failure is
    /// logged once.
    last_failure: Mutex<Option<String>>,
    pub damage: DamageHub,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            screen: Mutex::new(None),
            first_screen: Condvar::new(),
            mapping: Mutex::new(None),
            last_failure: Mutex::new(None),
            // The hub learns the real size with the first screen, before
            // anything can subscribe.
            damage: DamageHub::new(Size {
                width: 1,
                height: 1,
            }),
        }
    }

    /// The screen, locked.
    pub fn screen(&self) -> Result<MutexGuard<'_, Option<Framebuffer>>> {
        self.screen
            .lock()
            .map_err(|_| anyhow!("the qemu screen's lock is poisoned"))
    }

    /// Wait up to `timeout` for qemu's first screen, and return its size.
    pub fn wait_for_screen(&self, timeout: Duration) -> Result<Size> {
        let screen = self.screen()?;
        let (screen, _) = self
            .first_screen
            .wait_timeout_while(screen, timeout, |screen| screen.is_none())
            .map_err(|_| anyhow!("the qemu screen's lock is poisoned"))?;
        match screen.as_ref() {
            Some(screen) => Ok(screen.size()),
            None => bail!(
                "qemu sent no picture of the guest's screen within {}s of andris registering",
                timeout.as_secs()
            ),
        }
    }

    fn mapping(&self) -> Result<MutexGuard<'_, Option<ScanoutMmap>>> {
        self.mapping
            .lock()
            .map_err(|_| anyhow!("the qemu mapping's lock is poisoned"))
    }

    /// Make the screen `size`, from `source`, and report it all damaged:
    /// as a resize if the size changed.
    fn replace(&self, size: Size, source: Source<'_>) -> Result<()> {
        let resized = {
            let mut screen = self.screen()?;
            let resized = screen.as_ref().map(Framebuffer::size) != Some(size);
            match screen.as_mut() {
                Some(screen) => screen.replace(size, source)?,
                None => {
                    let mut fresh = Framebuffer::new(size)?;
                    fresh.replace(size, source)?;
                    *screen = Some(fresh);
                    self.first_screen.notify_all();
                }
            }
            resized
        };
        if resized {
            self.damage.resize(size)
        } else {
            self.damage.damage(&[Rectangle::full(size)])
        }
    }

    /// Copy `rect` with `copy`, and report what of it lies on the screen
    /// as damage.
    fn update(
        &self,
        rect: Rectangle,
        copy: impl FnOnce(&mut Framebuffer) -> Result<Option<Rectangle>>,
    ) -> Result<()> {
        let damaged = {
            let mut screen = self.screen()?;
            let Some(screen) = screen.as_mut() else {
                bail!("qemu sent an update of {rect} before any screen");
            };
            copy(screen)?
        };
        match damaged {
            Some(rect) => self.damage.damage(&[rect]),
            None => Ok(()),
        }
    }

    /// Log `result`'s failure as a warning, unless it is the one logged
    /// last; a success clears that.
    fn report(&self, what: &str, result: Result<()>) {
        let Ok(mut last) = self.last_failure.lock() else {
            return;
        };
        match result {
            Ok(()) => *last = None,
            Err(error) => {
                let message = format!("{error:#}");
                if last.as_deref() != Some(message.as_str()) {
                    warn!("cannot apply qemu's {what}, so the picture may be stale: {message}");
                    *last = Some(message);
                }
            }
        }
    }
}

/// A rectangle from qemu's signed coordinates, or `None` if it has no
/// pixels at non-negative coordinates. Negative parts are cut off.
fn rectangle(x: i32, y: i32, w: i32, h: i32) -> Option<Rectangle> {
    let (right, bottom) = (i64::from(x) + i64::from(w), i64::from(y) + i64::from(h));
    let (x, y) = (x.max(0), y.max(0));
    if right <= i64::from(x) || bottom <= i64::from(y) {
        return None;
    }
    let width = u32::try_from(right - i64::from(x)).ok()?;
    let height = u32::try_from(bottom - i64::from(y)).ok()?;
    Some(Rectangle::new(x as u32, y as u32, width, height))
}

/// Now, on the wall clock, in microseconds since the epoch: the clock
/// qemu's trace log stamps events with.
fn wall_clock_us() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_micros())
}

/// `org.qemu.Display1.Listener`: pixels in the message.
pub struct Listener {
    shared: Arc<Shared>,
}

impl Listener {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

#[zbus::interface(name = "org.qemu.Display1.Listener", spawn = false)]
impl Listener {
    /// The whole screen, which replaces any screen qemu shared.
    async fn scanout(&mut self, width: u32, height: u32, stride: u32, format: u32, data: ByteBuf) {
        let received = wall_clock_us();
        let started = Instant::now();
        let result = self.shared.mapping().map(|mut mapping| mapping.take());
        let result = result.and_then(|_| {
            self.shared.replace(
                Size { width, height },
                Source {
                    data: &data,
                    stride: stride as usize,
                    format,
                },
            )
        });
        debug!(
            kind = "Scanout",
            width,
            height,
            stride,
            format = format_args!("{format:#010x}"),
            received_us = received,
            copy_us = started.elapsed().as_micros(),
            "qemu screen"
        );
        self.shared.report("Scanout", result);
    }

    /// One rectangle of the screen, its pixels starting at its top left.
    #[allow(clippy::too_many_arguments)]
    async fn update(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        stride: u32,
        format: u32,
        data: ByteBuf,
    ) {
        let received = wall_clock_us();
        let started = Instant::now();
        let Some(rect) = rectangle(x, y, w, h) else {
            return;
        };
        let result = self.shared.update(rect, |screen| {
            screen.update_from_patch(
                rect,
                Source {
                    data: &data,
                    stride: stride as usize,
                    format,
                },
            )
        });
        debug!(
            kind = "Update",
            x,
            y,
            w,
            h,
            bytes = data.len(),
            received_us = received,
            copy_us = started.elapsed().as_micros(),
            "qemu update"
        );
        self.shared.report("Update", result);
    }

    /// The guest turned its screen off. The last picture stays.
    async fn disable(&mut self) {
        debug!("qemu disabled the guest's screen; the last picture stays");
    }

    /// The optional interfaces this listener implements, which qemu reads
    /// once, when the listener registers.
    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        vec![MAP_INTERFACE.to_string()]
    }
}

/// `org.qemu.Display1.Listener.Unix.Map`: the screen in shared memory.
pub struct MapListener {
    shared: Arc<Shared>,
}

impl MapListener {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.Map", spawn = false)]
impl MapListener {
    /// qemu shares the screen: `fd` holds it from `offset` on. qemu waits
    /// for this to return, with its main loop held. Failing makes qemu
    /// send pixels in the message from then on.
    async fn scanout_map(
        &mut self,
        fd: Fd<'_>,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> fdo::Result<()> {
        let received = wall_clock_us();
        let started = Instant::now();
        let fd = fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| fdo::Error::Failed(format!("cannot keep the screen's fd: {error}")))?;
        let mapped = ScanoutMap {
            fd,
            offset,
            width,
            height,
            stride,
            format,
        }
        .mmap()
        .map_err(|error| fdo::Error::Failed(format!("cannot map the screen: {error}")))?;
        let result = self.shared.mapping().and_then(|mut mapping| {
            let result = self.shared.replace(
                Size { width, height },
                Source {
                    data: mapped.as_ref(),
                    stride: stride as usize,
                    format,
                },
            );
            *mapping = Some(mapped);
            result
        });
        debug!(
            kind = "ScanoutMap",
            width,
            height,
            stride,
            format = format_args!("{format:#010x}"),
            received_us = received,
            copy_us = started.elapsed().as_micros(),
            "qemu screen"
        );
        self.shared.report("ScanoutMap", result);
        Ok(())
    }

    /// A rectangle of the shared screen changed.
    async fn update_map(&mut self, x: i32, y: i32, w: i32, h: i32) -> fdo::Result<()> {
        let received = wall_clock_us();
        let started = Instant::now();
        let Some(rect) = rectangle(x, y, w, h) else {
            return Ok(());
        };
        let result = self.shared.mapping().and_then(|mapping| {
            let Some(mapped) = mapping.as_ref() else {
                bail!("qemu sent UpdateMap with no screen shared");
            };
            self.shared.update(rect, |screen| {
                screen.update_from_screen(
                    rect,
                    Source {
                        data: mapped.as_ref(),
                        stride: mapped.stride() as usize,
                        format: mapped.format(),
                    },
                )
            })
        });
        debug!(
            kind = "UpdateMap",
            x,
            y,
            w,
            h,
            received_us = received,
            copy_us = started.elapsed().as_micros(),
            "qemu update"
        );
        self.shared.report("UpdateMap", result);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_lose_their_negative_parts() {
        assert_eq!(rectangle(1, 2, 3, 4), Some(Rectangle::new(1, 2, 3, 4)));
        assert_eq!(rectangle(-1, -2, 3, 4), Some(Rectangle::new(0, 0, 2, 2)));
        assert_eq!(rectangle(-3, 0, 3, 4), None);
        assert_eq!(rectangle(0, 0, 0, 4), None);
        assert_eq!(rectangle(0, 0, -5, 4), None);
        assert_eq!(
            rectangle(i32::MAX, 0, i32::MAX, 1),
            Some(Rectangle::new(i32::MAX as u32, 0, i32::MAX as u32, 1))
        );
    }
}
