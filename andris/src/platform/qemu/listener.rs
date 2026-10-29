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
//! The cursor comes on the same interface: `CursorDefine` gives a shape
//! in ARGB words, and `MouseSet` shows or hides it. A guest that draws
//! its own pointer into the screen, as one on std-vga does, sends
//! neither. `MouseSet` also carries the pointer's position, which
//! andris has no use for: its clients draw the pointer where their own
//! mouse is.
//!
//! Each update is logged at debug level with the wall-clock time it
//! arrived, in microseconds since the epoch, and how long the copy took,
//! so that a trace can be lined up with qemu's own trace log, which
//! stamps events with the same clock.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use qemu_display::{ScanoutMap, ScanoutMmap};
use serde_bytes::ByteBuf;
use tokio::sync::watch;
use tracing::{debug, warn};
use zbus::fdo;
use zbus::zvariant::Fd;

use super::framebuffer::{Framebuffer, Source};
use crate::platform::{CursorReceiver, CursorShape, DamageHub, Rectangle, Size};

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
    /// The guest's cursor as qemu last described it.
    cursor: Mutex<Cursor>,
    /// The cursor as the platform reports it: the shape while qemu shows
    /// it, otherwise `None`.
    cursor_shapes: watch::Sender<Option<Arc<CursorShape>>>,
}

/// The guest's cursor: the shape qemu last defined, and whether qemu
/// shows it. Shown until qemu says otherwise.
struct Cursor {
    shape: Option<Arc<CursorShape>>,
    visible: bool,
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
            cursor: Mutex::new(Cursor {
                shape: None,
                visible: true,
            }),
            cursor_shapes: watch::channel(None).0,
        }
    }

    /// The screen's size. Fails if qemu has sent no screen yet.
    pub fn size(&self) -> Result<Size> {
        self.screen()?
            .as_ref()
            .map(Framebuffer::size)
            .ok_or_else(|| anyhow!("qemu has sent no screen"))
    }

    /// A receiver of the cursor as the platform reports it.
    pub fn subscribe_cursor(&self) -> CursorReceiver {
        self.cursor_shapes.subscribe()
    }

    /// Change the cursor with `change`, and report the result if what
    /// clients should see changed.
    fn set_cursor(&self, change: impl FnOnce(&mut Cursor)) -> Result<()> {
        let mut cursor = self
            .cursor
            .lock()
            .map_err(|_| anyhow!("the qemu cursor's lock is poisoned"))?;
        change(&mut cursor);
        let shown = cursor.shape.clone().filter(|_| cursor.visible);
        // MouseSet comes with every move of the guest's pointer, so the
        // shape is compared by identity, not pixel by pixel; a define
        // makes a new one.
        self.cursor_shapes
            .send_if_modified(|reported| match (reported.as_ref(), shown.as_ref()) {
                (None, None) => false,
                (Some(old), Some(new)) if Arc::ptr_eq(old, new) => false,
                _ => {
                    *reported = shown;
                    true
                }
            });
        Ok(())
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

/// The cursor qemu's `CursorDefine` describes: `width` x `height` with
/// its hot spot at (`hot_x`, `hot_y`), and `data` holding each pixel as a
/// 32-bit ARGB word in qemu's byte order, which is this host's.
fn cursor_shape(
    width: i32,
    height: i32,
    hot_x: i32,
    hot_y: i32,
    data: &[u8],
) -> Result<CursorShape> {
    let side = |value: i32, what: &str| {
        u16::try_from(value).with_context(|| format!("qemu sent a cursor {what} of {value}"))
    };
    let (width, height) = (side(width, "width")?, side(height, "height")?);
    let (hot_x, hot_y) = (side(hot_x, "hot spot x")?, side(hot_y, "hot spot y")?);
    let pixels = usize::from(width) * usize::from(height);
    ensure!(
        data.len() == pixels * 4,
        "qemu sent {} bytes for a {width}x{height} cursor, not {}",
        data.len(),
        pixels * 4
    );
    let argb = data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_ne_bytes(*word))
        .collect();
    CursorShape::new(width, height, hot_x, hot_y, argb)
}

/// `fd`, if the file behind it holds at least `needed` bytes. Reading a
/// mapping past the end of its file faults, which would kill andris, so
/// a screen is mapped only once the file is known to hold it. qemu does
/// not seal its screens' memfds against shrinking, so a qemu that
/// truncated one later could still fault andris; the guest cannot.
fn holding(fd: OwnedFd, needed: u64) -> Result<OwnedFd> {
    let file = std::fs::File::from(fd);
    let length = file
        .metadata()
        .context("cannot read the size of the screen's file")?
        .len();
    ensure!(
        length >= needed,
        "qemu shared a screen of {needed} bytes in a file of {length}"
    );
    Ok(OwnedFd::from(file))
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
        // The pixels start at qemu's corner, which a negative coordinate
        // put off the screen: the first on it is this far in.
        let origin = (
            (i64::from(rect.x) - i64::from(x)) as u32,
            (i64::from(rect.y) - i64::from(y)) as u32,
        );
        let result = self.shared.update(rect, |screen| {
            screen.update_from_patch(
                rect,
                Source {
                    data: &data,
                    stride: stride as usize,
                    format,
                },
                origin,
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

    /// The guest's cursor took a new shape.
    async fn cursor_define(
        &mut self,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        data: ByteBuf,
    ) {
        debug!(width, height, hot_x, hot_y, "qemu cursor shape");
        let result = cursor_shape(width, height, hot_x, hot_y, &data).and_then(|shape| {
            let shape = Arc::new(shape);
            self.shared.set_cursor(|cursor| cursor.shape = Some(shape))
        });
        // Only a failure is reported: a success would clear a failing
        // screen update's warning, and log it again at its next failure.
        if result.is_err() {
            self.shared.report("CursorDefine", result);
        }
    }

    /// The guest's pointer moved to (`x`, `y`), and is shown unless `on`
    /// is 0. Only whether it is shown matters to andris.
    async fn mouse_set(&mut self, _x: i32, _y: i32, on: i32) {
        let result = self.shared.set_cursor(|cursor| cursor.visible = on != 0);
        if result.is_err() {
            self.shared.report("MouseSet", result);
        }
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
        let needed = u64::from(offset) + u64::from(stride) * u64::from(height);
        let fd = holding(fd, needed).map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
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
        // A screen andris refuses is not kept, so that no update is read
        // from it against another screen's geometry.
        let result = self.shared.mapping().and_then(|mut mapping| {
            *mapping = None;
            self.shared.replace(
                Size { width, height },
                Source {
                    data: mapped.as_ref(),
                    stride: stride as usize,
                    format,
                },
            )?;
            *mapping = Some(mapped);
            Ok(())
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
    fn a_screen_must_fit_its_file() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(100).unwrap();
        let fd = OwnedFd::from(file);
        let fd = holding(fd, 100).unwrap();
        let error = holding(fd, 101).unwrap_err();
        assert!(format!("{error:#}").contains("101 bytes in a file of 100"));
    }

    #[test]
    fn cursors_take_qemus_argb_words() {
        let words = [0xff00_00ffu32, 0x8012_3456, 0, 0x0102_0304];
        let data: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
        let shape = cursor_shape(2, 2, 1, 1, &data).unwrap();
        assert_eq!(
            (shape.width(), shape.height(), shape.hot_x(), shape.hot_y()),
            (2, 2, 1, 1)
        );
        assert_eq!(shape.argb(), words);

        let refused = |width, height, hot_x, hot_y, data: &[u8]| {
            format!(
                "{:#}",
                cursor_shape(width, height, hot_x, hot_y, data).unwrap_err()
            )
        };
        assert!(refused(2, 2, 0, 0, &data[..12]).contains("12 bytes for a 2x2 cursor, not 16"));
        assert!(refused(2, 2, 0, 0, &[data.clone(), vec![0; 4]].concat()).contains("20 bytes"));
        assert!(refused(-2, 2, 0, 0, &data).contains("width of -2"));
        assert!(refused(2, 70_000, 0, 0, &data).contains("height of 70000"));
        assert!(refused(2, 2, -1, 0, &data).contains("hot spot x of -1"));
        assert!(refused(2, 2, 0, 2, &data).contains("hot spot"));
        assert!(refused(0, 0, 0, 0, &[]).contains("no pixels"));
    }

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
