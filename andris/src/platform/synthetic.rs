//! A synthetic platform: a deterministic test pattern, an input sink
//! that records the most recent [`RECORDED_EVENTS`] events it is given,
//! and one cursor shape.
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
//! The unused fourth byte of each pixel is 0. Each row carries
//! [`STRIDE_PADDING`] bytes of padding, filled with [`PADDING_BYTE`], so
//! that code which mistakes the stride for `width * 4` shows up.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, anyhow};
use tokio::sync::watch;

use super::{
    CursorReceiver, CursorShape, CursorSource, DisplayGeometry, EvdevCode, Frame, FrameSource,
    InputEvent, InputSink, LockState, Platform, Size,
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

/// What the synthetic desktop looks like now.
#[derive(Debug)]
struct State {
    size: Size,
    square_x: u32,
    locks: LockState,
}

/// The synthetic platform. Create it with [`SyntheticPlatform::new`] and
/// hand the server [`SyntheticPlatform::platform`]; keep the `Arc` to
/// drive and observe it.
#[derive(Debug)]
pub struct SyntheticPlatform {
    state: Mutex<State>,
    events: Mutex<VecDeque<InputEvent>>,
    cursor: watch::Sender<Option<Arc<CursorShape>>>,
}

impl SyntheticPlatform {
    /// A desktop of `size`, with the square at the left edge, every lock
    /// off and the [`default_cursor`] as its cursor. Fails if `size` is
    /// refused by [`Size::check`].
    pub fn new(size: Size) -> Result<Arc<Self>> {
        size.check()?;
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                size,
                square_x: 0,
                locks: LockState::default(),
            }),
            events: Mutex::new(VecDeque::new()),
            cursor: watch::Sender::new(Some(Arc::new(default_cursor()))),
        }))
    }

    /// This platform as all four seams.
    pub fn platform(self: &Arc<Self>) -> Platform {
        Platform {
            geometry: self.clone(),
            frames: self.clone(),
            input: self.clone(),
            cursor: self.clone(),
        }
    }

    /// Move the square right by [`SQUARE_STEP`] pixels, back to the left
    /// edge once it would cross the right one.
    pub fn advance(&self) -> Result<()> {
        let mut state = self.state()?;
        let next = state.square_x + SQUARE_STEP;
        state.square_x = if next + SQUARE_SIZE > state.size.width {
            0
        } else {
            next
        };
        Ok(())
    }

    /// The square's left edge.
    pub fn square_x(&self) -> Result<u32> {
        Ok(self.state()?.square_x)
    }

    /// Resize the desktop, keeping the square where it is if it still
    /// fits and moving it to the left edge otherwise. Fails if `size` is
    /// refused by [`Size::check`].
    pub fn set_size(&self, size: Size) -> Result<()> {
        size.check()?;
        let mut state = self.state()?;
        state.size = size;
        if state.square_x + SQUARE_SIZE > size.width {
            state.square_x = 0;
        }
        Ok(())
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

    fn events(&self) -> Result<MutexGuard<'_, VecDeque<InputEvent>>> {
        self.events
            .lock()
            .map_err(|_| anyhow!("the synthetic platform's event lock is poisoned"))
    }
}

impl DisplayGeometry for SyntheticPlatform {
    fn size(&self) -> Result<Size> {
        Ok(self.state()?.size)
    }
}

impl FrameSource for SyntheticPlatform {
    fn capture(&self) -> Result<Frame> {
        let (size, square_x) = {
            let state = self.state()?;
            (state.size, state.square_x)
        };
        render(size, square_x)
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

/// Draw the pattern at `size` with the square's left edge at `square_x`.
fn render(size: Size, square_x: u32) -> Result<Frame> {
    let row_bytes = size.width as usize * 4;
    let stride = row_bytes + STRIDE_PADDING;
    let mut data = vec![PADDING_BYTE; stride * size.height as usize];
    let square_fits =
        square_x + SQUARE_SIZE <= size.width && SQUARE_TOP + SQUARE_SIZE <= size.height;
    for (y, row) in (0..size.height).zip(data.chunks_exact_mut(stride)) {
        let in_square_rows = square_fits && (SQUARE_TOP..SQUARE_TOP + SQUARE_SIZE).contains(&y);
        for (x, pixel) in (0..size.width).zip(row[..row_bytes].as_chunks_mut::<4>().0) {
            let (r, g, b) = if in_square_rows && (square_x..square_x + SQUARE_SIZE).contains(&x) {
                SQUARE
            } else {
                // x < width, so the index is at most 7.
                BARS[(u64::from(x) * 8 / u64::from(size.width)) as usize]
            };
            *pixel = [b, g, r, 0];
        }
    }
    Frame::new(size.width, size.height, stride, data)
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
        let frame = platform.capture().unwrap();
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
        assert_eq!(a.capture().unwrap(), b.capture().unwrap());
    }

    #[test]
    fn pattern_has_bars_and_the_square() {
        let platform = SyntheticPlatform::new(SIZE).unwrap();
        let frame = platform.capture().unwrap();
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
        let before = platform.capture().unwrap();
        platform.advance().unwrap();
        assert_eq!(platform.square_x().unwrap(), SQUARE_STEP);
        let after = platform.capture().unwrap();
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
        let frame = platform.capture().unwrap();
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
        assert_eq!(platform.capture().unwrap().size(), smaller);
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
