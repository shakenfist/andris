//! The platform seams: what andris needs from the desktop it exports.
//!
//! Seven traits separate the SPICE server from the desktop:
//!
//! - [`DisplayGeometry`] reports the size of the desktop;
//! - [`ScreenSource`] reports where the desktop changed, as damage, and
//!   captures parts of it, or all of it, as BGRX;
//! - [`InputSink`] injects keys, pointer motion and buttons, and reads
//!   the lock-key state;
//! - [`CursorSource`] yields the cursor shape as it changes;
//! - [`DisplayControl`] asks the desktop to change its size;
//! - [`Clipboard`] follows the text on the desktop's clipboard, and puts
//!   text there;
//! - [`AudioSource`] captures the desktop's sound, in 10 ms frames.
//!
//! [`Platform`] bundles one implementation of each, with a [`GoneSignal`]
//! that is set, with the reason, once the desktop has gone for good: when
//! the connection to the X display fails, say. [`synthetic`] is a
//! deterministic implementation for tests and load testing, and [`x11`]
//! exports an X display. With the `qemu` feature, `qemu` exports a
//! guest's screen from qemu's D-Bus display. Sound does not come from the
//! display system, so it is a seam of its own: [`pulse`] captures it from
//! the session's sound server, beside the X11 platform, and the synthetic
//! platform plays a tone. Audio is optional, and a platform may have
//! none.
//!
//! # Threads
//!
//! Every trait is `Send + Sync`, and a [`Platform`] shares each one as an
//! `Arc<dyn ...>`, so any task may hold any seam. The methods are
//! synchronous, and each says whether it may block:
//!
//! - a method that **may block** (an X round trip, a capture) is called
//!   from async code through `tokio::task::spawn_blocking`;
//! - [`InputSink::inject`] and [`InputSink::inject_release`] **never
//!   block**: an implementation only queues the event. They can be called
//!   from async code directly, and from a `Drop` implementation;
//! - [`CursorSource::subscribe`] never blocks, and its receiver is
//!   awaited;
//! - [`ScreenSource::subscribe_damage`] never blocks, and its
//!   [`DamageSubscription::changed`] is awaited;
//! - [`DisplayControl::set_size`] **may block**, for as long as the
//!   desktop takes to answer;
//! - [`Clipboard::changes`] and [`Clipboard::set`] never block: the
//!   receiver is awaited, and `set` only queues the text;
//! - [`AudioSource::open`] never blocks: capture starts on a thread or
//!   task of its own, and its receiver is awaited.
//!
//! [`damage`] holds the types damage is reported in, and how it is
//! accumulated. [`audio`] holds the audio frame, and the queue that
//! carries frames from capture to the server.
//!
//! This module knows nothing of SPICE. Mapping SPICE's scancodes, button
//! ids and modifier flags onto these types is the server's job, as is
//! speaking the guest agent protocol that resizes and clipboard text
//! arrive in.

pub mod audio;
pub mod damage;
pub mod pulse;
#[cfg(feature = "qemu")]
pub mod qemu;
pub mod synthetic;
pub mod x11;

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
// Plain constants, so that a frame andris captures is never larger than
// what the encoders and ryll's decoders accept. No ryll type crosses this
// module's interface.
use shakenfist_spice_compression::{MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS};
use tokio::sync::watch;

pub use audio::{
    AUDIO_CHANNELS, AUDIO_FRAME_BYTES, AUDIO_FRAME_DURATION, AUDIO_FRAME_SAMPLES,
    AUDIO_FRAME_VALUES, AUDIO_QUEUE_FRAMES, AUDIO_RATE, AudioFrame, AudioReceiver, AudioSender,
    audio_queue,
};
pub use damage::{DamageHub, DamageSubscription, Rectangle, Region, TILE};

/// The most clipboard text andris carries in either direction, in bytes.
/// Larger text is dropped, with a warning, rather than copied.
pub const MAX_CLIPBOARD_TEXT: usize = 16 * 1024 * 1024;

/// A short fingerprint of clipboard `text` for debug logs, so that one text
/// can be followed across andris, its clients and other programs without
/// logging the text: the 32-bit FNV-1a hash of its UTF-8 bytes, as eight
/// hex digits. It is not a secret: a short text can be found from it by
/// trying candidates, which is why only debug logs carry it.
pub fn text_hash(text: &str) -> String {
    let hash = text.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    format!("{hash:08x}")
}

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

/// A BGRX image, of the whole desktop or one rectangle of it: 32 bits a
/// pixel, bytes B, G, R and an unused byte, rows `stride` bytes apart.
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

    /// A copy of the pixels of `rect`, with no row padding. Fails unless
    /// `rect` has pixels and lies within the frame.
    pub fn crop(&self, rect: Rectangle) -> Result<Frame> {
        ensure!(
            rect.lies_within(self.size),
            "{rect} does not lie within a {} frame",
            self.size
        );
        let row_bytes = rect.width as usize * 4;
        let left = rect.x as usize * 4;
        let mut data = Vec::with_capacity(row_bytes * rect.height as usize);
        for y in rect.y..rect.y + rect.height {
            let row = self
                .row(y)
                .with_context(|| format!("row {y} lies outside a {} frame", self.size))?;
            data.extend_from_slice(&row[left..left + row_bytes]);
        }
        Frame::new(rect.width, rect.height, row_bytes, data)
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

/// The pixels of one rectangle of the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    rect: Rectangle,
    pixels: Frame,
}

impl Patch {
    /// `pixels` as the contents of `rect`. Fails unless they are the
    /// rectangle's size.
    pub fn new(rect: Rectangle, pixels: Frame) -> Result<Self> {
        ensure!(
            pixels.size() == rect.size(),
            "a {} image cannot be the pixels of {rect}",
            pixels.size()
        );
        Ok(Self { rect, pixels })
    }

    /// Where on the desktop the pixels belong.
    pub fn rect(&self) -> Rectangle {
        self.rect
    }

    /// The pixels, exactly the rectangle's size.
    pub fn pixels(&self) -> &Frame {
        &self.pixels
    }

    /// The pixels, consuming the patch.
    pub fn into_pixels(self) -> Frame {
        self.pixels
    }
}

/// What a capture read: the desktop's size when it was read, and the
/// pixels of each rectangle asked for, clipped to that size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    size: Size,
    patches: Vec<Patch>,
}

impl Capture {
    /// `patches` of a desktop of `size`. Fails unless every patch lies
    /// within it.
    pub fn new(size: Size, patches: Vec<Patch>) -> Result<Self> {
        for patch in &patches {
            ensure!(
                patch.rect.lies_within(size),
                "{} does not lie within a {size} desktop",
                patch.rect
            );
        }
        Ok(Self { size, patches })
    }

    /// One patch of all of `frame`, as [`ScreenSource::capture_all`]
    /// returns.
    pub fn whole(frame: Frame) -> Result<Self> {
        let size = frame.size();
        Self::new(size, vec![Patch::new(Rectangle::full(size), frame)?])
    }

    /// The parts of `frame`, a capture of the whole desktop, that `rects`
    /// cover, each clipped to it. A rectangle wholly outside it yields no
    /// patch.
    pub fn cut(frame: &Frame, rects: &[Rectangle]) -> Result<Self> {
        let size = frame.size();
        let patches = rects
            .iter()
            .filter_map(|rect| rect.clip(size))
            .map(|rect| Patch::new(rect, frame.crop(rect)?))
            .collect::<Result<_>>()?;
        Self::new(size, patches)
    }

    /// The desktop's size when it was captured.
    pub fn size(&self) -> Size {
        self.size
    }

    /// The patches, in the order their rectangles were asked for.
    pub fn patches(&self) -> &[Patch] {
        &self.patches
    }

    /// The patches, consuming the capture.
    pub fn into_patches(self) -> Vec<Patch> {
        self.patches
    }

    /// The whole desktop as one frame. Fails unless the capture is one
    /// patch covering all of it, as [`ScreenSource::capture_all`]
    /// returns.
    pub fn into_whole(self) -> Result<Frame> {
        let full = Rectangle::full(self.size);
        match <[Patch; 1]>::try_from(self.patches) {
            Ok([patch]) if patch.rect == full => Ok(patch.pixels),
            Ok([patch]) => bail!(
                "a capture of {} is not all of a {} desktop",
                patch.rect,
                self.size
            ),
            Err(patches) => bail!(
                "a capture of {} patches is not one of the whole desktop",
                patches.len()
            ),
        }
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

/// The error a seam returns once the desktop is gone for good: the
/// connection to it has failed, or the thread serving it has stopped.
/// Retrying cannot help. Any other error may be transient; a capture of a
/// desktop resized while it was read fails, say, and the next succeeds.
/// [`is_gone`] finds this in an error's chain.
#[derive(Debug)]
pub struct Gone(pub String);

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Gone {}

/// Whether `error`, or anything in its chain, is [`Gone`].
pub fn is_gone(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Gone>())
}

/// Sets a [`GoneSignal`]: says the desktop has gone for good, and why.
/// Clones set the same signal.
#[derive(Debug, Clone)]
pub struct GoneSender(Arc<watch::Sender<Option<String>>>);

impl GoneSender {
    /// Say the desktop has gone, because of `why`. Only the first reason
    /// given is kept; returns whether this was it. Never blocks.
    pub fn set(&self, why: String) -> bool {
        self.0.send_if_modified(|gone| {
            if gone.is_some() {
                return false;
            }
            *gone = Some(why);
            true
        })
    }
}

/// Learns that the desktop has gone for good: the connection to it has
/// failed, so nothing andris serves can work again. It is set once, with
/// the first reason a [`GoneSender`] gave, and stays set.
#[derive(Debug, Clone)]
pub struct GoneSignal(watch::Receiver<Option<String>>);

impl GoneSignal {
    /// A signal not yet set, and its sender.
    pub fn new() -> (GoneSender, GoneSignal) {
        let (sender, receiver) = watch::channel(None);
        (GoneSender(Arc::new(sender)), GoneSignal(receiver))
    }

    /// Why the desktop has gone, or `None` while it has not.
    pub fn why(&self) -> Option<String> {
        self.0.borrow().clone()
    }

    /// Wait until the desktop has gone, and return why. Returns at once
    /// if it already has. Never returns if every sender is dropped
    /// without setting it, since then nothing can say the desktop went.
    pub async fn wait(&mut self) -> String {
        // Bound first, so that the borrow wait_for returns is not held
        // across the await below.
        let why = match self.0.wait_for(Option::is_some).await {
            Ok(why) => why.clone(),
            Err(_) => None,
        };
        match why {
            Some(why) => why,
            None => std::future::pending().await,
        }
    }
}

/// Reports the size of the desktop.
pub trait DisplayGeometry: Send + Sync {
    /// The desktop's current size. May block.
    fn size(&self) -> Result<Size>;
}

/// Reports where the desktop changed, and captures it.
///
/// Damage says where to look; it may cover pixels that did not change.
/// A consumer waits on its subscription, takes the pending damage, and
/// then captures the rectangles it covers: in that order, so that damage
/// reported meanwhile stays pending for the next capture (see
/// [`damage`]).
///
/// For the captures, an error that is not [`Gone`] may pass: a later
/// capture can succeed.
pub trait ScreenSource: Send + Sync {
    /// A new subscription to the desktop's damage, whose pending damage
    /// starts as the whole desktop. Never blocks. Fails with [`Gone`]
    /// once the desktop has gone.
    fn subscribe_damage(&self) -> Result<DamageSubscription>;

    /// Capture `rects`, each clipped to the desktop's size when it is
    /// read; a rectangle wholly outside it yields no patch. The cost is
    /// proportional to the rectangles' area, not the desktop's. May
    /// block for as long as the capture takes.
    fn capture(&self, rects: &[Rectangle]) -> Result<Capture>;

    /// Capture the whole desktop at its current size, as one patch: for
    /// a new client, or after a resize. The size and pixels are read
    /// together, so they always agree. May block for as long as the
    /// capture takes.
    fn capture_all(&self) -> Result<Capture>;
}

/// Injects input into the desktop.
///
/// An implementation's queue may be bounded, so that a client flooding
/// input cannot grow it without limit. A release andris owes the desktop
/// must not be lost to that bound, or a key would stay held: those go
/// through [`InputSink::inject_release`], for which an implementation
/// keeps room beyond what [`InputSink::inject`] may fill.
pub trait InputSink: Send + Sync {
    /// Queue one event. Never blocks; events are applied in the order
    /// they are queued, whichever method queued them. An error means the
    /// sink can take no more input: its queue is full, or its connection
    /// to the display has gone.
    fn inject(&self, event: InputEvent) -> Result<()>;

    /// Queue the release of a key or button andris pressed: one held when
    /// a channel closes, or the second half of a lock-key toggle. Never
    /// blocks, and is applied after every event already queued. Unlike
    /// [`InputSink::inject`] it does not fail merely because the queue is
    /// full. Callers pass only releases, so there are never more of these
    /// queued than keys and buttons held.
    fn inject_release(&self, event: InputEvent) -> Result<()> {
        self.inject(event)
    }

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
                self.inject_release(InputEvent::Key {
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

/// What became of a [`DisplayControl::set_size`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resize {
    /// The desktop is now the size asked for, or the nearest size the
    /// desktop allows. The change reaches subscribers as a resize, as any
    /// other does.
    Applied,
    /// The desktop was already that size; nothing changed.
    Unchanged,
    /// The desktop does not allow the change, for the reason given, and
    /// was left as it was.
    Refused(String),
}

/// Changes the size of the desktop.
pub trait DisplayControl: Send + Sync {
    /// Make the desktop `size`. May block, for as long as the desktop
    /// takes to answer. A desktop that will not change says so with
    /// [`Resize::Refused`]; an error means the request could not be made
    /// at all, and is [`Gone`] once the desktop has gone.
    fn set_size(&self, size: Size) -> Result<Resize>;
}

/// The receiving end of a [`Clipboard`]: the text another program on the
/// desktop last put on its clipboard, `None` when there is none or it is
/// not text, and each change as it happens.
pub type ClipboardReceiver = watch::Receiver<Option<Arc<str>>>;

/// The desktop's clipboard, as text.
///
/// Only text that other programs on the desktop put there is reported:
/// text andris sets with [`Clipboard::set`] is not reported back to it,
/// and once it is on the clipboard the receiver holds `None`, as the
/// text another program put there has gone.
/// A program that copies the same text again is reported again, as a
/// change to the same value.
pub trait Clipboard: Send + Sync {
    /// A receiver that holds the desktop's current text and sees every
    /// later change. Never blocks. Intermediate texts may be skipped;
    /// only the latest matters.
    fn changes(&self) -> ClipboardReceiver;

    /// Put `text` on the desktop's clipboard, for other programs to paste.
    /// Never blocks: the text is queued. An error means the clipboard can
    /// take no more, because its connection to the desktop has gone.
    fn set(&self, text: Arc<str>) -> Result<()>;
}

/// Captures the desktop's sound.
///
/// Capture runs only while someone listens: [`AudioSource::open`] starts
/// it, and dropping the receiver it returns stops it, so the sound server
/// is not held, nor a sink kept awake, with nobody listening. Each open is
/// a fresh capture, with a queue of its own.
///
/// Frames are [`AUDIO_FRAME_SAMPLES`] samples a channel, S16 stereo at
/// [`AUDIO_RATE`], paced by the source's clock: a sound server's, or a
/// timer's. Each carries its capture time, and within one receiver those
/// times never go backwards. When the consumer falls behind, the receiver
/// drops the oldest frames and counts them (see [`audio`]).
///
/// Sound failing must never end a session, so failures are not errors of
/// the seam: a capture that cannot start (no sound server, a source that
/// does not exist) or that fails part way is logged once, as a warning,
/// and closes the receiver. Opening again tries again.
pub trait AudioSource: Send + Sync {
    /// Start capturing, and return the receiver frames arrive on. Never
    /// blocks: connecting to a sound server happens on the capture's own
    /// thread or task. Fails only when capture cannot even be started (a
    /// thread cannot be spawned, say); a capture that starts and then
    /// fails closes the receiver instead.
    fn open(&self) -> Result<AudioReceiver>;
}

/// One implementation of each seam, shared by the server's tasks, and the
/// signal that the desktop behind them has gone.
#[derive(Clone)]
pub struct Platform {
    pub geometry: Arc<dyn DisplayGeometry>,
    pub screen: Arc<dyn ScreenSource>,
    pub input: Arc<dyn InputSink>,
    pub cursor: Arc<dyn CursorSource>,
    pub display_control: Arc<dyn DisplayControl>,
    pub clipboard: Arc<dyn Clipboard>,
    /// The desktop's sound, or `None` when andris captures none
    /// (`--no-audio`).
    pub audio: Option<Arc<dyn AudioSource>>,
    /// Set once the desktop has gone for good; `andris` exits when it is.
    pub gone: GoneSignal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_hash_is_fnv1a_32() {
        // The published FNV-1a test vectors, which the e2e pasteboard's
        // hash must match too.
        assert_eq!(text_hash(""), "811c9dc5");
        assert_eq!(text_hash("a"), "e40c292c");
        assert_eq!(text_hash("foobar"), "bf9cf968");
    }

    #[test]
    fn gone_is_found_under_context() {
        let gone = anyhow::Error::new(Gone("the X11 capture thread has stopped".to_string()))
            .context("capturing the desktop");
        assert!(is_gone(&gone));
        assert!(format!("{gone:#}").contains("capture thread has stopped"));
        let passing = anyhow::anyhow!("BadMatch").context("capturing the desktop");
        assert!(!is_gone(&passing));
    }

    /// Whether `signal` returns within a moment.
    async fn returns_soon(signal: &GoneSignal) -> bool {
        let mut probe = signal.clone();
        tokio::time::timeout(std::time::Duration::from_millis(50), probe.wait())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn the_gone_signal_keeps_the_first_reason() {
        let (sender, mut signal) = GoneSignal::new();
        assert_eq!(signal.why(), None);
        assert!(!returns_soon(&signal).await, "an unset signal waits");

        let mut waiter = signal.clone();
        let woken = tokio::spawn(async move { waiter.wait().await });
        assert!(sender.clone().set("first".to_string()));
        assert!(!sender.set("second".to_string()));
        assert_eq!(woken.await.unwrap(), "first");
        assert_eq!(signal.wait().await, "first", "a set signal returns at once");
        assert_eq!(signal.why().as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn an_abandoned_gone_signal_never_returns() {
        let (sender, signal) = GoneSignal::new();
        drop(sender);
        assert!(!returns_soon(&signal).await);
    }

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
    fn crop_copies_a_rectangle_without_padding() {
        // Three rows of two pixels, stride 12 (4 bytes of padding).
        let data: Vec<u8> = (0..36).collect();
        let frame = Frame::new(2, 3, 12, data).unwrap();
        let cropped = frame.crop(Rectangle::new(1, 1, 1, 2)).unwrap();
        assert_eq!(
            cropped.size(),
            Size {
                width: 1,
                height: 2
            }
        );
        assert_eq!(cropped.stride(), 4);
        assert_eq!(cropped.data(), &[16, 17, 18, 19, 28, 29, 30, 31]);
        assert_eq!(
            frame.crop(Rectangle::full(frame.size())).unwrap().row(2),
            frame.row(2)
        );
        assert!(frame.crop(Rectangle::new(1, 1, 2, 1)).is_err());
        assert!(frame.crop(Rectangle::new(0, 0, 0, 1)).is_err());
    }

    #[test]
    fn patch_and_capture_checks() {
        let frame = Frame::new(2, 2, 8, vec![0; 16]).unwrap();
        assert!(Patch::new(Rectangle::new(5, 5, 2, 2), frame.clone()).is_ok());
        assert!(Patch::new(Rectangle::new(0, 0, 2, 1), frame.clone()).is_err());
        let patch = Patch::new(Rectangle::new(5, 5, 2, 2), frame.clone()).unwrap();
        let small = Size {
            width: 6,
            height: 7,
        };
        assert!(Capture::new(small, vec![patch.clone()]).is_err());
        let size = Size {
            width: 7,
            height: 7,
        };
        let capture = Capture::new(size, vec![patch]).unwrap();
        assert!(capture.into_whole().is_err(), "a part is not the whole");
        assert!(Capture::new(size, vec![]).unwrap().into_whole().is_err());
        assert_eq!(
            Capture::whole(frame.clone()).unwrap().into_whole().unwrap(),
            frame
        );
    }

    #[test]
    fn cut_clips_and_drops_rectangles() {
        let data: Vec<u8> = (0..64).collect();
        let frame = Frame::new(4, 4, 16, data).unwrap();
        let capture = Capture::cut(
            &frame,
            &[
                Rectangle::new(2, 2, 10, 10),
                Rectangle::new(9, 9, 1, 1),
                Rectangle::new(0, 0, 1, 1),
            ],
        )
        .unwrap();
        assert_eq!(capture.size(), frame.size());
        let rects: Vec<Rectangle> = capture.patches().iter().map(Patch::rect).collect();
        assert_eq!(
            rects,
            vec![Rectangle::new(2, 2, 2, 2), Rectangle::new(0, 0, 1, 1)]
        );
        assert_eq!(
            capture.patches()[0].pixels().data(),
            &[
                40, 41, 42, 43, 44, 45, 46, 47, 56, 57, 58, 59, 60, 61, 62, 63
            ]
        );
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
