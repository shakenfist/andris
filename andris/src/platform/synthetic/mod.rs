//! A synthetic platform: a deterministic test pattern, an input sink
//! that records the most recent [`RECORDED_EVENTS`] events it is given,
//! one cursor shape, display control that resizes the pattern, a
//! clipboard held in memory, and a tone for its sound.
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
//! [`SyntheticPlatform::animate`] shows one more rectangle above
//! everything: the same photograph, panned a little further on each call,
//! as a playing video's area changes. It is one rectangle, not an
//! overlay, so animating for long costs nothing extra; animating
//! another rectangle moves it there.
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
//! [`SyntheticPlatform::paint_photo`] the rectangle they paint,
//! [`SyntheticPlatform::animate`] the rectangle it animates and any it
//! moved from, and [`SyntheticPlatform::set_size`] the whole desktop at
//! its new size.
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
//! [`SyntheticPlatform::clipboard_set`] and is not reported back; it
//! replaces what was copied, so `None` is reported in its place.
//!
//! # The tone
//!
//! [`ToneSource`] is the platform's sound: a [`TONE_FREQUENCY`] sine that
//! sounds and falls silent in turn, as its [`TonePattern`] says, paced at
//! one frame every 10 ms by the async runtime's clock. The platform's
//! default sounds for [`DEFAULT_TONE_ON`] and is silent for
//! [`DEFAULT_TONE_OFF`]; a test that wants another pattern puts a
//! [`ToneSource`] of its own in the [`Platform`]'s `audio`.
//!
//! # Reporting changes
//!
//! Every change is reported while the state lock is still held, so
//! damage and resizes reach the hub in the order the changes were made:
//! two racing [`SyntheticPlatform::set_size`] calls cannot leave the hub
//! believing in the size that lost. A capture woken by the damage waits
//! for the lock, so it sees the change. The lock order is the state lock,
//! then the hub's locks; the hub never calls back into the platform, so
//! nothing takes them the other way round.

#[cfg(test)]
mod tests;
mod tone;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, anyhow, bail};
use tokio::sync::watch;

use super::{
    Backlog, Capture, Clipboard, ClipboardReceiver, CursorReceiver, CursorShape, CursorSource,
    DamageHub, DamageSubscription, DisplayControl, DisplayGeometry, EvdevCode, Frame, Gone,
    GoneSender, GoneSignal, INPUT_QUEUE_LIMIT, InputEvent, InputSink, LockState,
    MAX_CLIPBOARD_TEXT, Patch, Platform, RELEASE_RESERVE, Rectangle, Resize, ScreenSource, Size,
    SizeRange, Verdict, clamp, verdict,
};
pub use tone::{
    DEFAULT_TONE_OFF, DEFAULT_TONE_ON, TONE_AMPLITUDE, TONE_FREQUENCY, TonePattern, ToneSource,
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

/// The animated rectangle, and how many times it has moved on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Animation {
    rect: Rectangle,
    frame: u32,
}

/// What the synthetic desktop looks like now.
#[derive(Debug)]
struct State {
    size: Size,
    square_x: u32,
    /// Solid rectangles over the pattern, the latest on top.
    overlays: Vec<(Rectangle, Fill)>,
    /// Above the overlays.
    animation: Option<Animation>,
    locks: LockState,
    /// How many of the next captures fail.
    failing_captures: u32,
    /// How many of the next size reads fail.
    failing_size_reads: u32,
    /// Whether [`DisplayControl::set_size`] refuses.
    refusing_resizes: bool,
    /// The range [`DisplayControl::set_size`] clamps to, as a real
    /// server's `GetScreenSizeRange` does. `None` accepts any size
    /// [`Size::check`] allows, which is what a desktop with no range
    /// does.
    size_range: Option<SizeRange>,
    /// Whether injected events stay counted in the backlog, as they do
    /// on a real backend whose input thread is not keeping up. Off by
    /// default: with nothing holding them, a synthetic desktop applies
    /// each event as it is injected, so its queue is always empty.
    holding_input: bool,
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
    /// The bound on the input queue, as the real backends have.
    backlog: Backlog,
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
    animation: Option<Animation>,
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
                animation: None,
                locks: LockState::default(),
                failing_captures: 0,
                failing_size_reads: 0,
                refusing_resizes: false,
                size_range: None,
                holding_input: false,
                clipboard_set: None,
                clipboard_sets: 0,
            }),
            events: Mutex::new(VecDeque::new()),
            backlog: Backlog::new("synthetic"),
            cursor: watch::Sender::new(Some(Arc::new(default_cursor()))),
            clipboard: watch::Sender::new(None),
            damage: DamageHub::new(size),
            gone,
            gone_signal,
        }))
    }

    /// This platform as its seams, and its gone signal, with no sound.
    ///
    /// Sound is the configuration's to decide, and `run` sets it on
    /// whatever platform it opens, so a source installed here would be
    /// replaced on every path that serves a client. A test that wants
    /// the tone asks for
    /// [`SyntheticPlatform::platform_with_tone`].
    pub fn platform(self: &Arc<Self>) -> Platform {
        Platform {
            geometry: self.clone(),
            screen: self.clone(),
            input: self.clone(),
            cursor: self.clone(),
            display_control: self.clone(),
            clipboard: Some(self.clone()),
            audio: None,
            gone: self.gone_signal.clone(),
        }
    }

    /// [`SyntheticPlatform::platform`] with a [`ToneSource`] of the
    /// default pattern as its sound, for tests that want a desktop that
    /// makes a noise.
    pub fn platform_with_tone(self: &Arc<Self>) -> Platform {
        Platform {
            audio: Some(Arc::new(ToneSource::default())),
            ..self.platform()
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

    /// Show the next frame of a panning photograph in `rect`, above
    /// everything else, and report it as damage. Each call pans it on, so
    /// every call changes every pixel of it. Animating another rectangle
    /// than the last call did moves the animation, and reports the old
    /// rectangle too, which shows what lies beneath again.
    pub fn animate(&self, rect: Rectangle) -> Result<()> {
        // Reported under the lock, as every change is: see the module.
        let mut state = self.state()?;
        let old = state.animation;
        state.animation = Some(Animation {
            rect,
            frame: old.map_or(0, |old| old.frame.wrapping_add(1)),
        });
        let moved_from = old.map(|old| old.rect).filter(|&old| old != rect);
        let damaged: Vec<Rectangle> = moved_from.into_iter().chain([rect]).collect();
        self.damage.damage(&damaged)
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

    /// Clamp [`DisplayControl::set_size`] to `range`, as a real server's
    /// `GetScreenSizeRange` does, or accept any size with `None`.
    ///
    /// With a range set, a size outside it is clamped and applied, and a
    /// size that clamps to the one the desktop already has is refused:
    /// the rule in [`resize`](super::resize), which the X11 backend
    /// follows through RandR.
    pub fn set_size_range(&self, range: Option<SizeRange>) -> Result<()> {
        self.state()?.size_range = range;
        Ok(())
    }

    /// Stop uncounting injected events, so that the input queue fills and
    /// [`InputSink::inject`] starts refusing, as a real backend does when
    /// its input thread cannot keep up; or start again, which uncounts
    /// everything held.
    ///
    /// A synthetic desktop applies each event as it is injected, so
    /// without this its queue is never anything but empty, and no test
    /// through this backend could reach a full one.
    pub fn hold_input(&self, hold: bool) -> Result<()> {
        // The drain runs under the lock [`SyntheticPlatform::queue`]
        // holds, so the two cannot interleave. Draining with the lock
        // dropped could consume a count that an injection in flight had
        // taken and was about to give back, and that injection's
        // `done()` would then uncount a queue already at zero.
        let mut state = self.state()?;
        state.holding_input = hold;
        if !hold {
            while self.backlog.queued() > 0 {
                self.backlog.done();
            }
        }
        Ok(())
    }

    /// How many injected events are counted against the queue's bound.
    pub fn queued_input(&self) -> usize {
        self.backlog.queued()
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

    /// [`Gone`] once [`SyntheticPlatform::vanish`] has been called, as
    /// every seam of a real backend reports once its desktop has gone.
    ///
    /// Without this the synthetic backend kept serving a desktop it had
    /// been told was gone, so no test through it could exercise the
    /// server's gone handling anywhere but a damage subscription.
    fn still_here(&self) -> Result<()> {
        match self.gone_signal.why() {
            Some(why) => Err(anyhow::Error::new(Gone(why))),
            None => Ok(()),
        }
    }

    /// A copy of what decides the pixels.
    fn scene(&self) -> Result<Scene> {
        let state = self.state()?;
        Ok(Scene {
            size: state.size,
            square_x: state.square_x,
            overlays: state.overlays.clone(),
            animation: state.animation,
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

    /// Count `event` against the queue's bound and apply it, giving the
    /// count straight back unless [`SyntheticPlatform::hold_input`] is
    /// holding it. The count is also given back if applying the event
    /// fails, so a refusal never costs the queue a slot.
    fn queue(&self, event: InputEvent, limit: usize) -> Result<()> {
        self.still_here()?;
        // The decision, the count and the event under one lock, which
        // [`SyntheticPlatform::hold_input`] takes before it drains: see
        // the comment there for what interleaving would cost.
        let mut state = self.state()?;
        self.backlog.take(limit)?;
        let holding = state.holding_input;
        let applied = self.apply(&mut state, event);
        if !holding || applied.is_err() {
            self.backlog.done();
        }
        applied
    }

    /// Toggle the locks `event` toggles, and record it, dropping the
    /// oldest once [`RECORDED_EVENTS`] are kept.
    fn apply(&self, state: &mut State, event: InputEvent) -> Result<()> {
        if let InputEvent::Key {
            code,
            pressed: true,
        } = event
        {
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
}

impl DisplayGeometry for SyntheticPlatform {
    fn size(&self) -> Result<Size> {
        self.still_here()?;
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
        self.still_here()?;
        let scene = self.scene_for_capture()?;
        let patches = rects
            .iter()
            .filter_map(|rect| rect.clip(scene.size))
            .map(|rect| Patch::new(rect, render(&scene, rect)?))
            .collect::<Result<_>>()?;
        Capture::new(scene.size, patches)
    }

    fn capture_all(&self) -> Result<Capture> {
        self.still_here()?;
        let scene = self.scene_for_capture()?;
        Capture::whole(render(&scene, Rectangle::full(scene.size))?)
    }
}

impl InputSink for SyntheticPlatform {
    /// Records the event, dropping the oldest once [`RECORDED_EVENTS`]
    /// are kept. A press of a lock key toggles that lock, as an X server
    /// does.
    ///
    /// Fails once the queue's bound is reached, as a real backend does,
    /// which only happens under
    /// [`SyntheticPlatform::hold_input`]: otherwise the event is applied
    /// as it is injected, so nothing is ever waiting.
    fn inject(&self, event: InputEvent) -> Result<()> {
        self.queue(event, INPUT_QUEUE_LIMIT)
    }

    /// As [`InputSink::inject`], but drawing on [`RELEASE_RESERVE`], so
    /// that a release andris owes the desktop gets through a full queue.
    fn inject_release(&self, event: InputEvent) -> Result<()> {
        self.queue(event, INPUT_QUEUE_LIMIT + RELEASE_RESERVE)
    }

    fn lock_state(&self) -> Result<LockState> {
        self.still_here()?;
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
    ///
    /// With a range set by [`SyntheticPlatform::set_size_range`], the
    /// size is clamped to it first and the verdict follows the rule in
    /// [`resize`](super::resize), so that clamping and at-the-limit
    /// behaviour can be tested through this backend as well as through
    /// RandR.
    fn set_size(&self, size: Size) -> Result<Resize> {
        self.still_here()?;
        // Reported under the lock, as every change is: see the module.
        let mut state = self.state()?;
        if state.refusing_resizes {
            return Ok(Resize::Refused(
                "the synthetic desktop is set to refuse resizes".to_string(),
            ));
        }
        let target = match state.size_range {
            Some(range) => clamp(size, range),
            None => size,
        };
        match verdict(size, target, state.size == target) {
            Verdict::Unchanged => return Ok(Resize::Unchanged),
            Verdict::AtLimit => {
                let range = state.size_range.map(|range| range.to_string());
                return Ok(Resize::Refused(format!(
                    "the synthetic desktop is already {target}, the nearest it allows to {size}{}",
                    range.map_or(String::new(), |range| format!(" (its range is {range})"))
                )));
            }
            Verdict::Change => {}
        }
        if let Err(error) = target.check() {
            return Ok(Resize::Refused(format!("{error:#}")));
        }
        resize_state(&mut state, target);
        self.damage.resize(target)?;
        Ok(Resize::Applied)
    }
}

impl Clipboard for SyntheticPlatform {
    fn changes(&self) -> ClipboardReceiver {
        self.clipboard.subscribe()
    }

    /// Keeps the text for [`SyntheticPlatform::clipboard_set`], and does
    /// not report it to [`Clipboard::changes`]; what another program
    /// copied is gone, so that is reported as `None`.
    ///
    /// Text over [`MAX_CLIPBOARD_TEXT`] is dropped with a warning and
    /// not kept, as the X11 backend drops it.
    fn set(&self, text: Arc<str>) -> Result<()> {
        self.still_here()?;
        if text.len() > MAX_CLIPBOARD_TEXT {
            tracing::warn!(
                bytes = text.len(),
                "not putting text on the synthetic clipboard: it is over the \
                 {MAX_CLIPBOARD_TEXT}-byte limit"
            );
            return Ok(());
        }
        let mut state = self.state()?;
        state.clipboard_set = Some(text);
        state.clipboard_sets = state.clipboard_sets.saturating_add(1);
        self.clipboard
            .send_if_modified(|held| held.take().is_some());
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
            let animation = scene
                .animation
                .filter(|animation| contains(&animation.rect, x, y));
            let overlay = || {
                scene
                    .overlays
                    .iter()
                    .rev()
                    .find(|(area, _)| contains(area, x, y))
            };
            let (r, g, b) = match animation {
                Some(animation) => animated_pixel(&animation, x, y),
                None => match overlay() {
                    Some(&(_, Fill::Solid(rgb))) => rgb,
                    Some((_, Fill::Photo)) => photo_pixel(x, y),
                    None if square.is_some_and(|square| contains(&square, x, y)) => SQUARE,
                    // x < width, so the index is at most 7.
                    None => BARS[(u64::from(x) * 8 / width) as usize],
                },
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

/// How far the animation pans before it starts again, each way: the
/// photograph of [`photo_pixel`] keeps its detail this far from its
/// corner, and goes flat further out, where its gradients saturate.
const ANIMATION_PERIOD: u32 = 256;

/// Pixel (`x`, `y`) of `animation`'s current frame: the photograph of
/// [`photo_pixel`], from the animated rectangle's corner, panned 7 pixels
/// left and 5 up a frame, grain and all, wrapping every
/// [`ANIMATION_PERIOD`] pixels. So nearly every pixel changes from one
/// frame to the next, as a panning camera's would.
fn animated_pixel(animation: &Animation, x: u32, y: u32) -> Rgb {
    let frame = animation.frame % ANIMATION_PERIOD;
    photo_pixel(
        (x - animation.rect.x + frame * 7) % ANIMATION_PERIOD,
        (y - animation.rect.y + frame * 5) % ANIMATION_PERIOD,
    )
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
