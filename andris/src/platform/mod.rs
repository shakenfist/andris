//! The platform seams: what andris needs from the desktop it exports.
//!
//! Four traits separate the SPICE server from the display system:
//!
//! - [`DisplayGeometry`] reports the size of the desktop;
//! - [`FrameSource`] captures the whole desktop as BGRX;
//! - [`InputSink`] injects keys, pointer motion and buttons, and reads
//!   the lock-key state;
//! - [`CursorSource`] yields the cursor shape as it changes.
//!
//! [`Platform`] bundles one implementation of each. [`synthetic`] is a
//! deterministic implementation for tests and load testing, and [`x11`]
//! exports an X display.
//!
//! # Threads
//!
//! Every trait is `Send + Sync`, and a [`Platform`] shares each one as an
//! `Arc<dyn ...>`, so any task may hold any seam. The methods are
//! synchronous, and each says whether it may block:
//!
//! - a method that **may block** (an X round trip, a capture) is called
//!   from async code through `tokio::task::spawn_blocking`;
//! - [`InputSink::inject`] **never blocks**: an implementation only
//!   queues the event. It can be called from async code directly, and
//!   from a `Drop` implementation;
//! - [`CursorSource::subscribe`] never blocks, and its receiver is
//!   awaited.
//!
//! This module knows nothing of SPICE. Mapping SPICE's scancodes, button
//! ids and modifier flags onto these types is the server's job.

pub mod synthetic;
pub mod x11;

use std::sync::Arc;

use anyhow::{Result, bail, ensure};
// Plain constants, so that a frame andris captures is never larger than
// what the encoders and ryll's decoders accept. No ryll type crosses this
// module's interface.
use shakenfist_spice_compression::{MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS};
use tokio::sync::watch;

/// The size of a desktop or frame, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    /// Fails unless both sides are at least 1 and at most
    /// `MAX_IMAGE_DIMENSION`, and the pixel count is at most
    /// `MAX_IMAGE_PIXELS`: the limits ryll's encoders and decoders
    /// apply.
    pub fn check(&self) -> Result<()> {
        ensure!(
            self.width > 0 && self.height > 0,
            "a {self} desktop has no pixels"
        );
        ensure!(
            self.width <= MAX_IMAGE_DIMENSION && self.height <= MAX_IMAGE_DIMENSION,
            "a {self} desktop has a side over {MAX_IMAGE_DIMENSION} pixels"
        );
        ensure!(
            self.width as usize * self.height as usize <= MAX_IMAGE_PIXELS,
            "a {self} desktop has more than {MAX_IMAGE_PIXELS} pixels"
        );
        Ok(())
    }
}

impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// A capture of the whole desktop: 32 bits a pixel, bytes B, G, R and an
/// unused byte, rows `stride` bytes apart.
///
/// The fields are private, so a `Frame` always satisfies what
/// [`Frame::new`] checks.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    size: Size,
    stride: usize,
    data: Vec<u8>,
}

impl Frame {
    /// Wrap `data` as a `width` x `height` BGRX frame whose rows start
    /// `stride` bytes apart.
    ///
    /// Fails if the size is refused by [`Size::check`], if `stride` is
    /// less than `width * 4`, or if `data` is too short to hold the last
    /// row. The last row needs only `width * 4` bytes, so a buffer with
    /// no trailing padding is accepted.
    pub fn new(width: u32, height: u32, stride: usize, data: Vec<u8>) -> Result<Self> {
        let size = Size { width, height };
        size.check()?;
        let row_bytes = width as usize * 4;
        ensure!(
            stride >= row_bytes,
            "a {size} frame needs a stride of at least {row_bytes} bytes, not {stride}"
        );
        let Some(needed) = stride
            .checked_mul(height as usize - 1)
            .and_then(|n| n.checked_add(row_bytes))
        else {
            bail!("a {size} frame with a stride of {stride} bytes is too large to address");
        };
        ensure!(
            data.len() >= needed,
            "a {size} frame with a stride of {stride} bytes needs {needed} bytes of data, not {}",
            data.len()
        );
        Ok(Self { size, stride, data })
    }

    pub fn width(&self) -> u32 {
        self.size.width
    }

    pub fn height(&self) -> u32 {
        self.size.height
    }

    pub fn size(&self) -> Size {
        self.size
    }

    /// Bytes from the start of one row to the start of the next.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// The pixel buffer, including any row padding.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Row `y` without its padding, `width * 4` bytes; `None` past the
    /// last row.
    pub fn row(&self, y: u32) -> Option<&[u8]> {
        self.band(y, 1)
    }

    /// The bytes of `count` rows starting at row `top`, including the
    /// padding between them but not after the last: a buffer of that
    /// many rows with this frame's width and stride. `None` if `count`
    /// is zero or the rows run past the bottom of the frame.
    pub fn band(&self, top: u32, count: u32) -> Option<&[u8]> {
        if count == 0 || top.checked_add(count)? > self.size.height {
            return None;
        }
        // Frame::new bounded the whole buffer, so these cannot overflow.
        let start = top as usize * self.stride;
        let end = (top + count - 1) as usize * self.stride + self.size.width as usize * 4;
        self.data.get(start..end)
    }
}

impl std::fmt::Debug for Frame {
    // The pixels would swamp any log or assertion message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("size", &self.size)
            .field("stride", &self.stride)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// A key, named by its Linux evdev code (`KEY_*` in
/// `linux/input-event-codes.h`). Evdev codes are the neutral key
/// identity because libei uses them too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EvdevCode(pub u16);

impl EvdevCode {
    /// `KEY_CAPSLOCK`.
    pub const CAPSLOCK: EvdevCode = EvdevCode(58);
    /// `KEY_NUMLOCK`.
    pub const NUMLOCK: EvdevCode = EvdevCode(69);
    /// `KEY_SCROLLLOCK`.
    pub const SCROLLLOCK: EvdevCode = EvdevCode(70);
}

/// A pointer button. Each backend numbers these its own way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    Side,
    Extra,
}

/// One input event for an [`InputSink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// A key pressed or released.
    Key { code: EvdevCode, pressed: bool },
    /// The pointer moved to a position on the desktop. The caller has
    /// already clamped it to the desktop's size.
    PointerAbsolute { x: u32, y: u32 },
    /// The pointer moved by an offset.
    PointerRelative { dx: i32, dy: i32 },
    /// A button pressed or released.
    Button { button: Button, pressed: bool },
}

/// Which lock keys are on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LockState {
    pub caps: bool,
    pub num: bool,
    pub scroll: bool,
}

/// A cursor image with its hot spot. Each pixel is a 32-bit ARGB word,
/// alpha in the top byte, rows top to bottom with no padding, as XFixes
/// reports a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    width: u16,
    height: u16,
    hot_x: u16,
    hot_y: u16,
    argb: Vec<u32>,
}

impl CursorShape {
    /// Fails if a side is zero, `argb` does not hold exactly
    /// `width * height` pixels, or the hot spot is outside the image.
    pub fn new(width: u16, height: u16, hot_x: u16, hot_y: u16, argb: Vec<u32>) -> Result<Self> {
        ensure!(
            width > 0 && height > 0,
            "a {width}x{height} cursor has no pixels"
        );
        let pixels = width as usize * height as usize;
        ensure!(
            argb.len() == pixels,
            "a {width}x{height} cursor needs {pixels} pixels, not {}",
            argb.len()
        );
        ensure!(
            hot_x < width && hot_y < height,
            "hot spot ({hot_x}, {hot_y}) is outside a {width}x{height} cursor"
        );
        Ok(Self {
            width,
            height,
            hot_x,
            hot_y,
            argb,
        })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn hot_x(&self) -> u16 {
        self.hot_x
    }

    pub fn hot_y(&self) -> u16 {
        self.hot_y
    }

    /// The pixels, `width * height` ARGB words.
    pub fn argb(&self) -> &[u32] {
        &self.argb
    }
}

/// Reports the size of the desktop.
pub trait DisplayGeometry: Send + Sync {
    /// The desktop's current size. May block.
    fn size(&self) -> Result<Size>;
}

/// Captures the desktop.
pub trait FrameSource: Send + Sync {
    /// Capture the whole desktop at its current size. May block for as
    /// long as a capture takes.
    fn capture(&self) -> Result<Frame>;
}

/// Injects input into the desktop.
pub trait InputSink: Send + Sync {
    /// Queue one event. Never blocks; events are applied in the order
    /// they are queued. An error means the sink can take no more input,
    /// for instance because its connection to the display has gone.
    fn inject(&self, event: InputEvent) -> Result<()>;

    /// The desktop's current lock state, read after every event already
    /// queued has been applied. May block.
    fn lock_state(&self) -> Result<LockState>;

    /// Make the desktop's lock state `wanted`, by pressing and releasing
    /// each lock key whose state differs. May block, because it reads
    /// [`InputSink::lock_state`] first.
    fn set_lock_state(&self, wanted: LockState) -> Result<()> {
        let current = self.lock_state()?;
        let locks = [
            (wanted.caps, current.caps, EvdevCode::CAPSLOCK),
            (wanted.num, current.num, EvdevCode::NUMLOCK),
            (wanted.scroll, current.scroll, EvdevCode::SCROLLLOCK),
        ];
        for (want, have, code) in locks {
            if want != have {
                self.inject(InputEvent::Key {
                    code,
                    pressed: true,
                })?;
                self.inject(InputEvent::Key {
                    code,
                    pressed: false,
                })?;
            }
        }
        Ok(())
    }
}

/// The receiving end of a [`CursorSource`]: the current shape, `None`
/// when there is none, and each change as it happens.
pub type CursorReceiver = watch::Receiver<Option<Arc<CursorShape>>>;

/// Yields the cursor shape as it changes.
pub trait CursorSource: Send + Sync {
    /// A receiver that holds the current shape and sees every later
    /// change. Never blocks. Intermediate shapes may be skipped; only the
    /// latest matters.
    fn subscribe(&self) -> CursorReceiver;
}

/// One implementation of each seam, shared by the server's tasks.
#[derive(Clone)]
pub struct Platform {
    pub geometry: Arc<dyn DisplayGeometry>,
    pub frames: Arc<dyn FrameSource>,
    pub input: Arc<dyn InputSink>,
    pub cursor: Arc<dyn CursorSource>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_limits() {
        assert!(
            Size {
                width: 1,
                height: 1
            }
            .check()
            .is_ok()
        );
        assert!(
            Size {
                width: 0,
                height: 768
            }
            .check()
            .is_err()
        );
        assert!(
            Size {
                width: MAX_IMAGE_DIMENSION + 1,
                height: 1
            }
            .check()
            .is_err()
        );
        assert!(
            Size {
                width: MAX_IMAGE_DIMENSION,
                height: MAX_IMAGE_DIMENSION
            }
            .check()
            .is_err(),
            "the pixel count cap also applies"
        );
    }

    #[test]
    fn frame_accepts_padded_and_unpadded_buffers() {
        assert!(Frame::new(4, 2, 16, vec![0; 32]).is_ok());
        // The last row needs no padding.
        assert!(Frame::new(4, 2, 20, vec![0; 36]).is_ok());
    }

    #[test]
    fn frame_refuses_bad_geometry() {
        assert!(Frame::new(0, 2, 16, vec![0; 32]).is_err());
        assert!(Frame::new(4, 2, 15, vec![0; 32]).is_err(), "short stride");
        assert!(Frame::new(4, 2, 20, vec![0; 35]).is_err(), "short data");
    }

    #[test]
    fn frame_rows_and_bands() {
        // Three rows of two pixels, stride 12 (4 bytes of padding).
        let data: Vec<u8> = (0..36).collect();
        let frame = Frame::new(2, 3, 12, data).unwrap();
        assert_eq!(frame.row(0).unwrap(), &(0..8).collect::<Vec<u8>>()[..]);
        assert_eq!(frame.row(2).unwrap(), &(24..32).collect::<Vec<u8>>()[..]);
        assert!(frame.row(3).is_none());
        assert_eq!(
            frame.band(1, 2).unwrap(),
            &(12..32).collect::<Vec<u8>>()[..]
        );
        assert!(frame.band(0, 0).is_none());
        assert!(frame.band(2, 2).is_none());
        assert!(frame.band(u32::MAX, 2).is_none());
    }

    #[test]
    fn cursor_shape_checks() {
        assert!(CursorShape::new(2, 2, 1, 1, vec![0; 4]).is_ok());
        assert!(CursorShape::new(0, 2, 0, 0, vec![]).is_err());
        assert!(CursorShape::new(2, 2, 0, 0, vec![0; 3]).is_err());
        assert!(CursorShape::new(2, 2, 2, 0, vec![0; 4]).is_err());
    }

    /// A sink that records events and reports a fixed lock state.
    struct Recorder {
        locks: LockState,
        events: std::sync::Mutex<Vec<InputEvent>>,
    }

    impl InputSink for Recorder {
        fn inject(&self, event: InputEvent) -> Result<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }

        fn lock_state(&self) -> Result<LockState> {
            Ok(self.locks)
        }
    }

    #[test]
    fn set_lock_state_presses_only_differing_locks() {
        let sink = Recorder {
            locks: LockState {
                caps: true,
                num: false,
                scroll: false,
            },
            events: Default::default(),
        };
        sink.set_lock_state(LockState {
            caps: true,
            num: true,
            scroll: false,
        })
        .unwrap();
        let events = sink.events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                InputEvent::Key {
                    code: EvdevCode::NUMLOCK,
                    pressed: true
                },
                InputEvent::Key {
                    code: EvdevCode::NUMLOCK,
                    pressed: false
                },
            ]
        );
    }
}
