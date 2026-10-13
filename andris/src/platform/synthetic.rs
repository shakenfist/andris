//! A synthetic platform: a deterministic test pattern, an input sink
//! that records the most recent [`RECORDED_EVENTS`] events it is given,
//! one cursor shape, display control that resizes the pattern, and a
//! clipboard held in memory.
//!
//! It backs `--platform synthetic`, for load testing, and the in-process
//! tests, which drive it through [`SyntheticPlatform`]'s own methods and
//! observe what the server did to it.
//!
//! # The pattern
//!
//! Eight vertical colour bars, white, yellow, cyan, green, magenta, red,
//! blue and black, each an eighth of the width. Over them sits a
//! [`SQUARE_SIZE`] pixel orange square whose top edge is at row
//! [`SQUARE_TOP`], which lies within rows 64 to 191, and so within the
//! second and third 64-row bands. [`SyntheticPlatform::advance`] moves it
//! right by [`SQUARE_STEP`] pixels, wrapping to the left edge, so a test
//! knows which bands changed. The square is left out when the desktop is
//! too small to hold it.
//!
//! The unused fourth byte of each pixel is 0. Each row of a captured
//! patch carries [`STRIDE_PADDING`] bytes of padding, filled with
//! [`PADDING_BYTE`], so that code which mistakes the stride for
//! `width * 4` shows up.
//!
//! [`SyntheticPlatform::paint`] and [`SyntheticPlatform::paint_undamaged`]
//! lay solid rectangles over the pattern, and
//! [`SyntheticPlatform::paint_photo`] a rectangle that looks like a
//! photograph: smooth gradients and waves with a fixed grain, which LZ4
//! shrinks little and JPEG a lot. The latest paint is on top.
//!
//! Paints are kept, not flattened into the pattern: each adds an overlay
//! that every later capture checks, pixel by pixel, so rendering costs
//! the pixels captured times the overlays painted. Paints are for tests
//! that make a few of them; a long load run should drive the desktop
//! with [`SyntheticPlatform::advance`], which costs nothing extra.
//!
//! # Damage
//!
//! [`SyntheticPlatform::advance`] reports the square's old and new
//! rectangles as damage, [`SyntheticPlatform::paint`] and
//! [`SyntheticPlatform::paint_photo`] the rectangle they paint, and
//! [`SyntheticPlatform::set_size`] the whole desktop at its new size.
//! [`SyntheticPlatform::paint_undamaged`] changes pixels and reports
//! nothing, as a change XDamage missed would, and
//! [`SyntheticPlatform::report_damage`] reports damage and changes
//! nothing. A capture renders only the rectangles asked for.
//! [`SyntheticPlatform::vanish`] makes the desktop go for good, as an X
//! server that exits does.
//!
//! # Display control and the clipboard
//!
//! [`DisplayControl::set_size`] resizes the pattern as
//! [`SyntheticPlatform::set_size`] does, unless
//! [`SyntheticPlatform::refuse_resizes`] has made it refuse, as an X
//! server that does not let clients resize it does.
//! [`SyntheticPlatform::copy_text`] puts text on the clipboard as if
//! another program had copied it, and reports it; text the server sets
//! through [`Clipboard::set`] is kept for
//! [`SyntheticPlatform::clipboard_set`] and is not reported back.
//!
//! Every change is reported while the state lock is still held, so
//! damage and resizes reach the hub in the order the changes were made:
//! two racing [`SyntheticPlatform::set_size`] calls cannot leave the hub
//! believing in the size that lost. A capture woken by the damage waits
//! for the lock, so it sees the change. The lock order is the state lock,
//! then the hub's locks; the hub never calls back into the platform, so
//! nothing takes them the other way round.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, anyhow, bail};
use tokio::sync::watch;

use super::{
    Capture, Clipboard, ClipboardReceiver, CursorReceiver, CursorShape, CursorSource, DamageHub,
    DamageSubscription, DisplayControl, DisplayGeometry, EvdevCode, Frame, GoneSender, GoneSignal,
    InputEvent, InputSink, LockState, Patch, Platform, Rectangle, Resize, ScreenSource, Size,
};

/// The side of the moving square, in pixels.
pub const SQUARE_SIZE: u32 = 64;
/// The square's top row.
pub const SQUARE_TOP: u32 = 96;
/// How far [`SyntheticPlatform::advance`] moves the square, in pixels.
pub const SQUARE_STEP: u32 = 8;
/// Bytes of padding after each row of a captured frame.
pub const STRIDE_PADDING: usize = 64;
/// The value of every padding byte.
pub const PADDING_BYTE: u8 = 0xAA;
/// How many of the most recent input events are kept; older ones are
/// dropped, so a long load test does not grow without limit.
pub const RECORDED_EVENTS: usize = 4096;

/// The bars' colours, left to right, as (R, G, B).
const BARS: [(u8, u8, u8); 8] = [
    (255, 255, 255),
    (255, 255, 0),
    (0, 255, 255),
    (0, 255, 0),
    (255, 0, 255),
    (255, 0, 0),
    (0, 0, 255),
    (0, 0, 0),
];

/// The square's colour, as (R, G, B); none of the bars has it.
const SQUARE: (u8, u8, u8) = (255, 128, 0);

/// The colour of a pixel, as (R, G, B).
pub type Rgb = (u8, u8, u8);

/// What a paint fills its rectangle with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// One colour.
    Solid(Rgb),
    /// Something like a photograph; see [`photo_pixel`].
    Photo,
}

/// What the synthetic desktop looks like now.
#[derive(Debug)]
struct State {
    size: Size,
    square_x: u32,
    /// Solid rectangles over the pattern, the latest on top.
    overlays: Vec<(Rectangle, Fill)>,
    locks: LockState,
    /// How many of the next captures fail.
    failing_captures: u32,
    /// How many of the next size reads fail.
    failing_size_reads: u32,
    /// Whether [`DisplayControl::set_size`] refuses.
    refusing_resizes: bool,
    /// The text the server last set on the clipboard, and how many times
    /// it has set any.
    clipboard_set: Option<Arc<str>>,
    clipboard_sets: u64,
}

/// The synthetic platform. Create it with [`SyntheticPlatform::new`] and
/// hand the server [`SyntheticPlatform::platform`]; keep the `Arc` to
/// drive and observe it.
#[derive(Debug)]
pub struct SyntheticPlatform {
    state: Mutex<State>,
    events: Mutex<VecDeque<InputEvent>>,
    cursor: watch::Sender<Option<Arc<CursorShape>>>,
    /// The text other programs put on the clipboard.
    clipboard: watch::Sender<Option<Arc<str>>>,
    damage: DamageHub,
    /// Set by [`SyntheticPlatform::vanish`].
    gone: GoneSender,
    /// What [`SyntheticPlatform::platform`] hands out.
    gone_signal: GoneSignal,
}

/// What a capture draws from: a copy of the parts of [`State`] that
/// decide pixels.
struct Scene {
    size: Size,
    square_x: u32,
    overlays: Vec<(Rectangle, Fill)>,
}

impl SyntheticPlatform {
    /// A desktop of `size`, with the square at the left edge, every lock
    /// off and the [`default_cursor`] as its cursor. Fails if `size` is
    /// refused by [`Size::check`].
    pub fn new(size: Size) -> Result<Arc<Self>> {
        size.check()?;
        let (gone, gone_signal) = GoneSignal::new();
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                size,
                square_x: 0,
                overlays: Vec::new(),
                locks: LockState::default(),
                failing_captures: 0,
                failing_size_reads: 0,
                refusing_resizes: false,
                clipboard_set: None,
                clipboard_sets: 0,
            }),
            events: Mutex::new(VecDeque::new()),
            cursor: watch::Sender::new(Some(Arc::new(default_cursor()))),
            clipboard: watch::Sender::new(None),
            damage: DamageHub::new(size),
            gone,
            gone_signal,
        }))
    }

    /// This platform as all six seams, and its gone signal.
    pub fn platform(self: &Arc<Self>) -> Platform {
        Platform {
            geometry: self.clone(),
            screen: self.clone(),
            input: self.clone(),
            cursor: self.clone(),
            display_control: self.clone(),
            clipboard: self.clone(),
            gone: self.gone_signal.clone(),
        }
    }

    /// Make the desktop go away for good, because of `why`, as an X
    /// server that exits does: set the platform's gone signal, and make
    /// every damage subscription fail with [`Gone`](super::Gone).
    pub fn vanish(&self, why: &str) -> Result<()> {
        self.gone.set(why.to_string());
        self.damage.gone(why.to_string())
    }

    /// Move the square right by [`SQUARE_STEP`] pixels, back to the left
    /// edge once it would cross the right one, and report its old and new
    /// rectangles as damage. A desktop too small to show the square
    /// reports none.
    pub fn advance(&self) -> Result<()> {
        let mut state = self.state()?;
        let old = square(state.size, state.square_x);
        let next = state.square_x + SQUARE_STEP;
        state.square_x = if next + SQUARE_SIZE > state.size.width {
            0
        } else {
            next
        };
        let new = square(state.size, state.square_x);
        if old != new {
            let damaged: Vec<Rectangle> = old.into_iter().chain(new).collect();
            self.damage.damage(&damaged)?;
        }
        Ok(())
    }

    /// Lay a solid `rgb` rectangle over the desktop, above the pattern,
    /// the square and earlier paints, without reporting damage: a change
    /// XDamage missed.
    pub fn paint_undamaged(&self, rect: Rectangle, rgb: Rgb) -> Result<()> {
        self.state()?.overlays.push((rect, Fill::Solid(rgb)));
        Ok(())
    }

    /// Lay a solid `rgb` rectangle over the desktop, as
    /// [`SyntheticPlatform::paint_undamaged`] does, and report it as
    /// damage.
    pub fn paint(&self, rect: Rectangle, rgb: Rgb) -> Result<()> {
        self.paint_damaged(rect, Fill::Solid(rgb))
    }

    /// Lay a rectangle that looks like a photograph over the desktop,
    /// and report it as damage. Its pixels depend only on their place on
    /// the desktop, so painting it again changes nothing.
    pub fn paint_photo(&self, rect: Rectangle) -> Result<()> {
        self.paint_damaged(rect, Fill::Photo)
    }

    fn paint_damaged(&self, rect: Rectangle, fill: Fill) -> Result<()> {
        // Reported under the lock, as every change is: see the module.
        let mut state = self.state()?;
        state.overlays.push((rect, fill));
        self.damage.damage(&[rect])
    }

    /// Report `rect` as damaged without changing a pixel.
    pub fn report_damage(&self, rect: Rectangle) -> Result<()> {
        self.damage.damage(&[rect])
    }

    /// The whole desktop as it is now. Unlike a capture, it never fails,
    /// and does not use up a failure set by
    /// [`SyntheticPlatform::fail_captures`].
    pub fn desktop(&self) -> Result<Frame> {
        let scene = self.scene()?;
        render(&scene, Rectangle::full(scene.size))
    }

    /// The square's left edge.
    pub fn square_x(&self) -> Result<u32> {
        Ok(self.state()?.square_x)
    }

    /// Resize the desktop, keeping the square where it is if it still
    /// fits and moving it to the left edge otherwise, and report the
    /// resize. Fails if `size` is refused by [`Size::check`].
    pub fn set_size(&self, size: Size) -> Result<()> {
        // Reported under the lock, as every change is: see the module.
        let _state = self.resize_scene(size)?;
        self.damage.resize(size)
    }

    /// Resize the desktop, as [`SyntheticPlatform::set_size`] does,
    /// without reporting it: a capture that finds the new size before
    /// damage says so, as X11's capture thread can.
    pub fn set_size_unreported(&self, size: Size) -> Result<()> {
        self.resize_scene(size).map(drop)
    }

    fn resize_scene(&self, size: Size) -> Result<MutexGuard<'_, State>> {
        size.check()?;
        let mut state = self.state()?;
        resize_state(&mut state, size);
        Ok(state)
    }

    /// Make [`DisplayControl::set_size`] refuse every resize, or stop it
    /// refusing, as an X server that does not let clients resize it.
    pub fn refuse_resizes(&self, refuse: bool) -> Result<()> {
        self.state()?.refusing_resizes = refuse;
        Ok(())
    }

    /// Put `text` on the clipboard, or empty it with `None`, as another
    /// program on the desktop would, and report it to
    /// [`Clipboard::changes`]. The same text again is reported again.
    pub fn copy_text(&self, text: Option<&str>) {
        self.clipboard.send_replace(text.map(Arc::from));
    }

    /// The text the server last put on the clipboard with
    /// [`Clipboard::set`], if any.
    pub fn clipboard_set(&self) -> Result<Option<Arc<str>>> {
        Ok(self.state()?.clipboard_set.clone())
    }

    /// How many times the server has put text on the clipboard.
    pub fn clipboard_sets(&self) -> Result<u64> {
        Ok(self.state()?.clipboard_sets)
    }

    /// The events injected so far, oldest first: at most the last
    /// [`RECORDED_EVENTS`].
    pub fn input_events(&self) -> Result<Vec<InputEvent>> {
        Ok(self.events()?.iter().copied().collect())
    }

    /// The events injected so far, oldest first, leaving none recorded:
    /// at most the last [`RECORDED_EVENTS`].
    pub fn take_input_events(&self) -> Result<Vec<InputEvent>> {
        Ok(std::mem::take(&mut *self.events()?).into())
    }

    /// Make the next `count` captures fail, as a capture of a desktop
    /// resized while it was read does. Both [`ScreenSource::capture`] and
    /// [`ScreenSource::capture_all`] count. The failures are not
    /// [`Gone`].
    ///
    /// [`Gone`]: super::Gone
    pub fn fail_captures(&self, count: u32) -> Result<()> {
        self.state()?.failing_captures = count;
        Ok(())
    }

    /// Make the next `count` size reads fail, as a read of a desktop
    /// being resized can. The failures are not [`Gone`].
    ///
    /// [`Gone`]: super::Gone
    pub fn fail_size_reads(&self, count: u32) -> Result<()> {
        self.state()?.failing_size_reads = count;
        Ok(())
    }

    /// Set the lock state directly, as if changed from outside andris.
    pub fn force_lock_state(&self, locks: LockState) -> Result<()> {
        self.state()?.locks = locks;
        Ok(())
    }

    /// Change the cursor shape; `None` means there is none.
    pub fn set_cursor(&self, shape: Option<CursorShape>) {
        self.cursor.send_replace(shape.map(Arc::new));
    }

    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("the synthetic platform's state lock is poisoned"))
    }

    /// A copy of what decides the pixels.
    fn scene(&self) -> Result<Scene> {
        let state = self.state()?;
        Ok(Scene {
            size: state.size,
            square_x: state.square_x,
            overlays: state.overlays.clone(),
        })
    }

    /// The scene for a capture, unless a failure is due.
    fn scene_for_capture(&self) -> Result<Scene> {
        {
            let mut state = self.state()?;
            if state.failing_captures > 0 {
                state.failing_captures -= 1;
                bail!("a synthetic capture failure");
            }
        }
        self.scene()
    }

    fn events(&self) -> Result<MutexGuard<'_, VecDeque<InputEvent>>> {
        self.events
            .lock()
            .map_err(|_| anyhow!("the synthetic platform's event lock is poisoned"))
    }
}

impl DisplayGeometry for SyntheticPlatform {
    fn size(&self) -> Result<Size> {
        let mut state = self.state()?;
        if state.failing_size_reads > 0 {
            state.failing_size_reads -= 1;
            bail!("a synthetic size read failure");
        }
        Ok(state.size)
    }
}

impl ScreenSource for SyntheticPlatform {
    fn subscribe_damage(&self) -> Result<DamageSubscription> {
        self.damage.subscribe()
    }

    fn capture(&self, rects: &[Rectangle]) -> Result<Capture> {
        let scene = self.scene_for_capture()?;
        let patches = rects
            .iter()
            .filter_map(|rect| rect.clip(scene.size))
            .map(|rect| Patch::new(rect, render(&scene, rect)?))
            .collect::<Result<_>>()?;
        Capture::new(scene.size, patches)
    }

    fn capture_all(&self) -> Result<Capture> {
        let scene = self.scene_for_capture()?;
        Capture::whole(render(&scene, Rectangle::full(scene.size))?)
    }
}

impl InputSink for SyntheticPlatform {
    /// Records the event, dropping the oldest once [`RECORDED_EVENTS`]
    /// are kept. A press of a lock key toggles that lock, as an X server
    /// does.
    fn inject(&self, event: InputEvent) -> Result<()> {
        if let InputEvent::Key {
            code,
            pressed: true,
        } = event
        {
            let mut state = self.state()?;
            match code {
                EvdevCode::CAPSLOCK => state.locks.caps = !state.locks.caps,
                EvdevCode::NUMLOCK => state.locks.num = !state.locks.num,
                EvdevCode::SCROLLLOCK => state.locks.scroll = !state.locks.scroll,
                _ => {}
            }
        }
        let mut events = self.events()?;
        if events.len() >= RECORDED_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
        Ok(())
    }

    fn lock_state(&self) -> Result<LockState> {
        Ok(self.state()?.locks)
    }
}

impl CursorSource for SyntheticPlatform {
    fn subscribe(&self) -> CursorReceiver {
        self.cursor.subscribe()
    }
}

impl DisplayControl for SyntheticPlatform {
    /// Resizes the pattern, reporting the resize, unless
    /// [`SyntheticPlatform::refuse_resizes`] says to refuse. A size
    /// [`Size::check`] refuses is refused too.
    fn set_size(&self, size: Size) -> Result<Resize> {
        // Reported under the lock, as every change is: see the module.
        let mut state = self.state()?;
        if state.refusing_resizes {
            return Ok(Resize::Refused(
                "the synthetic desktop is set to refuse resizes".to_string(),
            ));
        }
        if state.size == size {
            return Ok(Resize::Unchanged);
        }
        if let Err(error) = size.check() {
            return Ok(Resize::Refused(format!("{error:#}")));
        }
        resize_state(&mut state, size);
        self.damage.resize(size)?;
        Ok(Resize::Applied)
    }
}

impl Clipboard for SyntheticPlatform {
    fn changes(&self) -> ClipboardReceiver {
        self.clipboard.subscribe()
    }

    /// Keeps the text for [`SyntheticPlatform::clipboard_set`], and does
    /// not report it to [`Clipboard::changes`].
    fn set(&self, text: Arc<str>) -> Result<()> {
        let mut state = self.state()?;
        state.clipboard_set = Some(text);
        state.clipboard_sets = state.clipboard_sets.saturating_add(1);
        Ok(())
    }
}

/// Make `state` a desktop of `size`, keeping the square where it is if it
/// still fits and moving it to the left edge otherwise.
fn resize_state(state: &mut State, size: Size) {
    state.size = size;
    if state.square_x + SQUARE_SIZE > size.width {
        state.square_x = 0;
    }
}

/// The square's rectangle on a desktop of `size` with its left edge at
/// `square_x`, or `None` if it does not fit and so is not drawn.
fn square(size: Size, square_x: u32) -> Option<Rectangle> {
    (square_x + SQUARE_SIZE <= size.width && SQUARE_TOP + SQUARE_SIZE <= size.height)
        .then(|| Rectangle::new(square_x, SQUARE_TOP, SQUARE_SIZE, SQUARE_SIZE))
}

/// Whether pixel (`x`, `y`) lies in `rect`.
fn contains(rect: &Rectangle, x: u32, y: u32) -> bool {
    rect.intersects(&Rectangle::new(x, y, 1, 1))
}

/// Draw `rect` of `scene`, which must lie within it: the pattern, the
/// square if it fits, then the overlays.
fn render(scene: &Scene, rect: Rectangle) -> Result<Frame> {
    let row_bytes = rect.width as usize * 4;
    let stride = row_bytes + STRIDE_PADDING;
    let mut data = vec![PADDING_BYTE; stride * rect.height as usize];
    let square = square(scene.size, scene.square_x);
    let width = u64::from(scene.size.width);
    for (y, row) in (rect.y..).zip(data.chunks_exact_mut(stride)) {
        for (x, pixel) in (rect.x..).zip(row[..row_bytes].as_chunks_mut::<4>().0) {
            let overlay = scene
                .overlays
                .iter()
                .rev()
                .find(|(area, _)| contains(area, x, y));
            let (r, g, b) = match overlay {
                Some(&(_, Fill::Solid(rgb))) => rgb,
                Some((_, Fill::Photo)) => photo_pixel(x, y),
                None if square.is_some_and(|square| contains(&square, x, y)) => SQUARE,
                // x < width, so the index is at most 7.
                None => BARS[(u64::from(x) * 8 / width) as usize],
            };
            *pixel = [b, g, r, 0];
        }
    }
    Frame::new(rect.width, rect.height, stride, data)
}

/// Pixel (`x`, `y`) of a photograph-like paint: gradients and slow waves
/// in each channel, plus a grain of up to 6 either way that a hash of the
/// place decides.
fn photo_pixel(x: u32, y: u32) -> Rgb {
    let (fx, fy) = (f64::from(x), f64::from(y));
    let wave = 40.0 * (fx / 23.0).sin() * (fy / 31.0).cos();
    let mut hash = (u64::from(x) << 32 | u64::from(y)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    hash ^= hash >> 32;
    let channel = |base: f64, shift: u32| {
        let grain = ((hash >> shift) % 13) as f64 - 6.0;
        // Clamped to a byte first, so the cast cannot truncate.
        (base + fx * 0.2 + fy * 0.15 + wave + grain).clamp(0.0, 255.0) as u8
    };
    (channel(160.0, 0), channel(110.0, 8), channel(60.0, 16))
}

/// The synthetic cursor: a 32x32 arrow, white with a black outline,
/// pointing up and left, with its hot spot at the tip, (0, 0).
pub fn default_cursor() -> CursorShape {
    const SIDE: u16 = 32;
    const BLACK: u32 = 0xFF00_0000;
    const WHITE: u32 = 0xFFFF_FFFF;
    const CLEAR: u32 = 0;
    let mut argb = vec![CLEAR; SIDE as usize * SIDE as usize];
    let mut put = |x: usize, y: usize, colour: u32| {
        if let Some(pixel) = argb.get_mut(y * SIDE as usize + x) {
            *pixel = colour;
        }
    };
    // The head: a triangle whose rows widen from 1 pixel at the tip to
    // 13 at row 19.
    for y in 0..20 {
        let width = y * 2 / 3 + 1;
        for x in 0..width {
            let edge = x == 0 || x == width - 1 || y == 19;
            put(x, y, if edge { BLACK } else { WHITE });
        }
    }
    // The tail: a 4-pixel-wide diagonal stroke from the head to the
    // bottom right.
    for y in 16..30 {
        let left = y / 2 + 1;
        for x in left..left + 4 {
            let edge = x == left || x == left + 3 || y == 29;
            put(x, y, if edge { BLACK } else { WHITE });
        }
    }
    // Built directly rather than through CursorShape::new, which cannot
    // fail for these values; default_cursor_passes_the_shape_checks
    // proves it.
    CursorShape {
        width: SIDE,
        height: SIDE,
        hot_x: 0,
        hot_y: 0,
        argb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Region;

    const SIZE: Size = Size {
        width: 1024,
        height: 768,
    };

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let row = frame.row(y).unwrap();
        let at = x as usize * 4;
        row[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn capture_has_the_size_and_a_padded_stride() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let frame = platform.capture_all().unwrap().into_whole().unwrap();
        assert_eq!(frame.size(), SIZE);
        assert_eq!(platform.size().unwrap(), SIZE);
        assert_eq!(frame.stride(), 1024 * 4 + STRIDE_PADDING);
        let row = frame.stride();
        assert!(
            frame.data()[row - STRIDE_PADDING..row]
                .iter()
                .all(|&b| b == PADDING_BYTE)
        );
    }

    #[test]
    fn capture_is_deterministic() {
        let a = SyntheticPlatform::new(SIZE).unwrap();
        let b = SyntheticPlatform::new(SIZE).unwrap();
        assert_eq!(a.capture_all().unwrap(), b.capture_all().unwrap());
        assert_eq!(a.desktop().unwrap(), b.desktop().unwrap());
    }

    #[test]
    fn pattern_has_bars_and_the_square() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let frame = platform.desktop().unwrap();
        // BGRX: white, then yellow in the second bar, black in the last.
        assert_eq!(pixel(&frame, 0, 0), [255, 255, 255, 0]);
        assert_eq!(pixel(&frame, 128, 0), [0, 255, 255, 0]);
        assert_eq!(pixel(&frame, 1023, 767), [0, 0, 0, 0]);
        // The square, orange, at (0, SQUARE_TOP).
        assert_eq!(pixel(&frame, 0, SQUARE_TOP), [0, 128, 255, 0]);
        assert_eq!(pixel(&frame, 63, SQUARE_TOP + 63), [0, 128, 255, 0]);
        assert_eq!(pixel(&frame, 64, SQUARE_TOP), [255, 255, 255, 0]);
        assert_eq!(pixel(&frame, 0, SQUARE_TOP - 1), [255, 255, 255, 0]);
    }

    #[test]
    fn advance_changes_only_bands_one_and_two() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let before = platform.desktop().unwrap();
        platform.advance().unwrap();
        assert_eq!(platform.square_x().unwrap(), SQUARE_STEP);
        let after = platform.desktop().unwrap();
        let changed: Vec<u32> = (0..SIZE.height.div_ceil(64))
            .filter(|band| {
                let top = band * 64;
                let rows = 64.min(SIZE.height - top);
                before.band(top, rows) != after.band(top, rows)
            })
            .collect();
        assert_eq!(changed, vec![1, 2]);
    }

    #[test]
    fn advance_wraps_at_the_right_edge() {
        let platform = SyntheticPlatform::new(Size {
            width: 80,
            height: 200,
        })
        .unwrap();
        platform.advance().unwrap();
        platform.advance().unwrap();
        assert_eq!(platform.square_x().unwrap(), 16);
        platform.advance().unwrap();
        assert_eq!(platform.square_x().unwrap(), 0);
    }

    #[test]
    fn small_desktop_has_no_square() {
        let platform = SyntheticPlatform::new(Size {
            width: 32,
            height: 32,
        })
        .unwrap();
        let frame = platform.desktop().unwrap();
        assert_eq!(pixel(&frame, 0, 0), [255, 255, 255, 0]);
        platform.advance().unwrap();
        assert_eq!(platform.square_x().unwrap(), 0);
    }

    #[test]
    fn set_size_changes_geometry_and_capture() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        for _ in 0..10 {
            platform.advance().unwrap();
        }
        let smaller = Size {
            width: 100,
            height: 200,
        };
        platform.set_size(smaller).unwrap();
        assert_eq!(platform.size().unwrap(), smaller);
        assert_eq!(platform.desktop().unwrap().size(), smaller);
        assert_eq!(platform.square_x().unwrap(), 0, "the square moved back");
        assert!(
            platform
                .set_size(Size {
                    width: 0,
                    height: 1
                })
                .is_err()
        );
    }

    #[test]
    fn fail_captures_fails_that_many_of_either_kind() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        platform.fail_captures(3).unwrap();
        let rects = [Rectangle::new(0, 0, 8, 8)];
        let errors = [
            platform.capture_all().unwrap_err(),
            platform.capture(&rects).unwrap_err(),
            platform.capture_all().unwrap_err(),
        ];
        for error in errors {
            assert!(!crate::platform::is_gone(&error));
        }
        assert!(platform.desktop().is_ok(), "desktop() is not a capture");
        assert_eq!(platform.capture(&rects).unwrap().patches().len(), 1);
        assert_eq!(platform.capture_all().unwrap().size(), SIZE);
    }

    /// The pending damage, as the tiles' merged rectangles.
    fn damaged(subscription: &DamageSubscription) -> Vec<Rectangle> {
        subscription.take().unwrap().rectangles(usize::MAX)
    }

    #[test]
    fn advance_damages_the_old_and_new_squares() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let subscription = platform.subscribe_damage().unwrap();
        assert_eq!(damaged(&subscription), vec![Rectangle::full(SIZE)]);
        assert!(damaged(&subscription).is_empty(), "nothing yet");

        // 0..64 and 8..72 across, rows 96..160: tile columns 0 and 1,
        // tile rows 1 and 2.
        platform.advance().unwrap();
        let mut expected = Region::empty(SIZE);
        expected.add(Rectangle::new(0, SQUARE_TOP, SQUARE_SIZE, SQUARE_SIZE));
        expected.add(Rectangle::new(
            SQUARE_STEP,
            SQUARE_TOP,
            SQUARE_SIZE,
            SQUARE_SIZE,
        ));
        let region = subscription.take().unwrap();
        assert_eq!(region, expected);
        assert_eq!(region.rectangles(32), vec![Rectangle::new(0, 64, 128, 128)]);

        // The wrap damages both ends.
        let narrow = SyntheticPlatform::new(Size {
            width: 200,
            height: 200,
        })
        .unwrap();
        let subscription = narrow.subscribe_damage().unwrap();
        for _ in 0..17 {
            narrow.advance().unwrap();
        }
        subscription.take().unwrap();
        assert_eq!(narrow.square_x().unwrap(), 136);
        narrow.advance().unwrap();
        assert_eq!(narrow.square_x().unwrap(), 0);
        assert_eq!(
            damaged(&subscription),
            vec![
                Rectangle::new(0, 64, 64, 128),
                Rectangle::new(128, 64, 72, 128)
            ]
        );
    }

    #[test]
    fn advance_damages_nothing_without_a_square() {
        let platform = SyntheticPlatform::new(Size {
            width: 32,
            height: 32,
        })
        .unwrap();
        let subscription = platform.subscribe_damage().unwrap();
        subscription.take().unwrap();
        platform.advance().unwrap();
        assert!(damaged(&subscription).is_empty());
    }

    #[test]
    fn set_size_damages_the_whole_new_desktop() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let subscription = platform.subscribe_damage().unwrap();
        subscription.take().unwrap();
        let smaller = Size {
            width: 100,
            height: 200,
        };
        platform.set_size(smaller).unwrap();
        assert_eq!(subscription.take().unwrap(), Region::full(smaller));
    }

    #[test]
    fn set_size_unreported_changes_capture_and_reports_nothing() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let subscription = platform.subscribe_damage().unwrap();
        subscription.take().unwrap();
        let smaller = Size {
            width: 100,
            height: 200,
        };
        platform.set_size_unreported(smaller).unwrap();
        assert_eq!(platform.capture_all().unwrap().size(), smaller);
        assert!(damaged(&subscription).is_empty());
    }

    #[test]
    fn paint_undamaged_changes_pixels_and_reports_nothing() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let subscription = platform.subscribe_damage().unwrap();
        subscription.take().unwrap();
        let before = platform.desktop().unwrap();
        platform
            .paint_undamaged(Rectangle::new(500, 500, 10, 10), (1, 2, 3))
            .unwrap();
        // Over the square too, and a later paint on top of an earlier.
        platform
            .paint_undamaged(Rectangle::new(0, SQUARE_TOP, 4, 4), (9, 9, 9))
            .unwrap();
        platform
            .paint_undamaged(Rectangle::new(2, SQUARE_TOP, 4, 4), (7, 7, 7))
            .unwrap();
        let after = platform.desktop().unwrap();
        assert_ne!(before, after);
        assert_eq!(pixel(&after, 500, 500), [3, 2, 1, 0]);
        assert_eq!(pixel(&after, 509, 509), [3, 2, 1, 0]);
        assert_eq!(pixel(&after, 510, 509), pixel(&before, 510, 509));
        assert_eq!(pixel(&after, 1, SQUARE_TOP), [9, 9, 9, 0]);
        assert_eq!(pixel(&after, 2, SQUARE_TOP), [7, 7, 7, 0]);
        assert_eq!(pixel(&after, 6, SQUARE_TOP + 4), [0, 128, 255, 0]);
        assert!(damaged(&subscription).is_empty());
    }

    #[test]
    fn paints_report_their_damage() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let subscription = platform.subscribe_damage().unwrap();
        subscription.take().unwrap();
        platform
            .paint(Rectangle::new(10, 10, 4, 4), (1, 2, 3))
            .unwrap();
        assert_eq!(damaged(&subscription), vec![Rectangle::new(0, 0, 64, 64)]);
        assert_eq!(pixel(&platform.desktop().unwrap(), 12, 12), [3, 2, 1, 0]);

        let photo = Rectangle::new(128, 0, 128, 64);
        platform.paint_photo(photo).unwrap();
        assert_eq!(damaged(&subscription), vec![photo]);
        let desktop = platform.desktop().unwrap();
        // Not flat: neighbours differ.
        assert_ne!(pixel(&desktop, 130, 5), pixel(&desktop, 131, 5));
        // Painting it again changes no pixel.
        platform.paint_photo(photo).unwrap();
        assert_eq!(platform.desktop().unwrap(), desktop);
    }

    #[test]
    fn report_damage_changes_nothing() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let subscription = platform.subscribe_damage().unwrap();
        subscription.take().unwrap();
        let before = platform.desktop().unwrap();
        platform
            .report_damage(Rectangle::new(300, 300, 1, 1))
            .unwrap();
        assert_eq!(platform.desktop().unwrap(), before);
        assert_eq!(
            damaged(&subscription),
            vec![Rectangle::new(256, 256, 64, 64)]
        );
    }

    #[test]
    fn captured_patches_equal_the_desktop() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        platform.advance().unwrap();
        platform
            .paint_undamaged(Rectangle::new(60, 100, 30, 30), (10, 20, 30))
            .unwrap();
        let desktop = platform.desktop().unwrap();
        let rects = [
            Rectangle::new(0, 64, 128, 128),
            Rectangle::new(1000, 700, 100, 100),
            Rectangle::new(5, 7, 1, 1),
            Rectangle::new(2000, 0, 10, 10),
        ];
        let capture = platform.capture(&rects).unwrap();
        assert_eq!(capture.size(), SIZE);
        let got: Vec<Rectangle> = capture.patches().iter().map(Patch::rect).collect();
        assert_eq!(
            got,
            vec![
                Rectangle::new(0, 64, 128, 128),
                Rectangle::new(1000, 700, 24, 68),
                Rectangle::new(5, 7, 1, 1),
            ],
            "clipped, and the rectangle outside dropped"
        );
        for patch in capture.patches() {
            let pixels = patch.pixels();
            assert_eq!(
                pixels.stride(),
                pixels.width() as usize * 4 + STRIDE_PADDING
            );
            let expected = desktop.crop(patch.rect()).unwrap();
            for y in 0..pixels.height() {
                assert_eq!(pixels.row(y), expected.row(y), "{} row {y}", patch.rect());
            }
        }
        assert_eq!(
            platform.capture_all().unwrap().into_whole().unwrap(),
            desktop
        );
    }

    #[test]
    fn fail_size_reads_fails_that_many() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        platform.fail_size_reads(2).unwrap();
        for _ in 0..2 {
            let error = platform.size().unwrap_err();
            assert!(!crate::platform::is_gone(&error));
        }
        assert_eq!(platform.size().unwrap(), SIZE);
    }

    #[test]
    fn records_input_and_toggles_locks() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let seams = platform.platform();
        let events = [
            InputEvent::Key {
                code: EvdevCode(30),
                pressed: true,
            },
            InputEvent::PointerAbsolute { x: 5, y: 6 },
            InputEvent::Key {
                code: EvdevCode::CAPSLOCK,
                pressed: true,
            },
            InputEvent::Key {
                code: EvdevCode::CAPSLOCK,
                pressed: false,
            },
        ];
        for event in events {
            seams.input.inject(event).unwrap();
        }
        assert_eq!(platform.input_events().unwrap(), events);
        assert_eq!(
            seams.input.lock_state().unwrap(),
            LockState {
                caps: true,
                num: false,
                scroll: false
            }
        );
        assert_eq!(platform.take_input_events().unwrap(), events);
        assert!(platform.input_events().unwrap().is_empty());
    }

    #[test]
    fn only_the_most_recent_events_are_kept() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let extra = 10;
        for x in 0..(RECORDED_EVENTS + extra) as u32 {
            platform
                .inject(InputEvent::PointerAbsolute { x, y: 0 })
                .unwrap();
        }
        let events = platform.take_input_events().unwrap();
        assert_eq!(events.len(), RECORDED_EVENTS);
        assert_eq!(
            events.first(),
            Some(&InputEvent::PointerAbsolute {
                x: extra as u32,
                y: 0
            })
        );
        assert_eq!(
            events.last(),
            Some(&InputEvent::PointerAbsolute {
                x: (RECORDED_EVENTS + extra - 1) as u32,
                y: 0
            })
        );
    }

    #[test]
    fn set_lock_state_converges() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        platform
            .force_lock_state(LockState {
                caps: true,
                num: false,
                scroll: true,
            })
            .unwrap();
        let wanted = LockState {
            caps: false,
            num: true,
            scroll: true,
        };
        platform.set_lock_state(wanted).unwrap();
        assert_eq!(platform.lock_state().unwrap(), wanted);
        assert_eq!(platform.take_input_events().unwrap().len(), 4);
        platform.set_lock_state(wanted).unwrap();
        assert!(platform.input_events().unwrap().is_empty());
    }

    #[test]
    fn cursor_starts_with_the_arrow_and_follows_changes() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let mut cursor = platform.platform().cursor.subscribe();
        let first = cursor.borrow_and_update().clone().unwrap();
        assert_eq!(*first, default_cursor());
        assert_eq!((first.width(), first.height()), (32, 32));
        assert_eq!((first.hot_x(), first.hot_y()), (0, 0));
        assert!(!cursor.has_changed().unwrap());

        let dot = CursorShape::new(1, 1, 0, 0, vec![0xFF00_0000]).unwrap();
        platform.set_cursor(Some(dot.clone()));
        assert!(cursor.has_changed().unwrap());
        assert_eq!(*cursor.borrow_and_update().clone().unwrap(), dot);
        platform.set_cursor(None);
        assert!(cursor.borrow_and_update().is_none());
    }

    #[test]
    fn display_control_resizes_unless_refusing() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let control = platform.platform().display_control;
        let subscription = platform.subscribe_damage().unwrap();
        subscription.take().unwrap();
        let smaller = Size {
            width: 640,
            height: 480,
        };
        assert_eq!(control.set_size(smaller).unwrap(), Resize::Applied);
        assert_eq!(platform.size().unwrap(), smaller);
        assert_eq!(subscription.take().unwrap(), Region::full(smaller));
        assert_eq!(control.set_size(smaller).unwrap(), Resize::Unchanged);

        let zero = Size {
            width: 0,
            height: 480,
        };
        assert!(matches!(
            control.set_size(zero).unwrap(),
            Resize::Refused(_)
        ));

        platform.refuse_resizes(true).unwrap();
        assert!(matches!(
            control.set_size(SIZE).unwrap(),
            Resize::Refused(_)
        ));
        assert_eq!(
            platform.size().unwrap(),
            smaller,
            "a refusal changes nothing"
        );
        assert!(damaged(&subscription).is_empty());
        platform.refuse_resizes(false).unwrap();
        assert_eq!(control.set_size(SIZE).unwrap(), Resize::Applied);
    }

    #[test]
    fn the_clipboard_reports_copies_and_keeps_what_is_set() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let clipboard = platform.platform().clipboard;
        let mut changes = clipboard.changes();
        assert!(changes.borrow_and_update().is_none());

        platform.copy_text(Some("copied"));
        assert!(changes.has_changed().unwrap());
        assert_eq!(changes.borrow_and_update().as_deref(), Some("copied"));
        platform.copy_text(Some("copied"));
        assert!(
            changes.has_changed().unwrap(),
            "the same text again is a change"
        );
        changes.mark_unchanged();

        clipboard.set(Arc::from("pasted")).unwrap();
        assert!(
            !changes.has_changed().unwrap(),
            "andris's own text is not reported"
        );
        assert_eq!(platform.clipboard_set().unwrap().as_deref(), Some("pasted"));
        assert_eq!(platform.clipboard_sets().unwrap(), 1);

        platform.copy_text(None);
        assert!(changes.borrow_and_update().is_none());
    }

    #[test]
    fn default_cursor_passes_the_shape_checks() {
        let cursor = default_cursor();
        let checked = CursorShape::new(
            cursor.width(),
            cursor.height(),
            cursor.hot_x(),
            cursor.hot_y(),
            cursor.argb().to_vec(),
        )
        .unwrap();
        assert_eq!(checked, cursor);
    }

    #[test]
    fn default_cursor_is_opaque_at_the_tip_and_clear_in_the_corner() {
        let cursor = default_cursor();
        assert_eq!(cursor.argb()[0], 0xFF00_0000);
        assert_eq!(cursor.argb()[31], 0);
        assert!(cursor.argb().contains(&0xFFFF_FFFF));
    }
}
