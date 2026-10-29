//! Keyboard and mouse input for the qemu platform: each event becomes a
//! call to qemu's `org.qemu.Display1.Keyboard` or `Mouse` on the first
//! console.
//!
//! Events wait on one FIFO, which a thread of the platform's drains in
//! order, making one D-Bus call for each and waiting for its reply, so
//! that a press is never overtaken by its release. The FIFO is bounded
//! as the X11 platform's is: [`InputSink::inject`] fails once
//! [`INPUT_QUEUE_LIMIT`] events are waiting, and
//! [`InputSink::inject_release`] may queue [`RELEASE_RESERVE`] more. A
//! call qemu refuses is logged and dropped; the next event is sent.
//!
//! - **Keys** go as qnums, the codes qemu's keyboard takes, through the
//!   table in [`qnums`](super::qnums). A key with no qnum is dropped.
//! - **Pointer motion** follows qemu's guest mouse, which is absolute
//!   (a tablet) or relative (a PS/2 mouse), as its `IsAbsolute` property
//!   says. The property is read again at most every
//!   [`ABSOLUTE_MAX_AGE`], and after any refused motion, since the guest
//!   can change mouse at any time. A position for an absolute mouse is
//!   set as it is; an offset for one moves the pointer from where andris
//!   last put it. An offset for a relative mouse is sent as it is; a
//!   position for one is sent as the offset from the last position, so
//!   the first position only sets where the pointer is taken to be, and
//!   the guest's own pointer acceleration may scale what follows.
//! - **Buttons**, wheel steps among them, go as qemu's button numbers,
//!   which name the same buttons as [`Button`] does.
//! - **The lock state** is the keyboard's `Modifiers` property, read in
//!   turn on the same thread, so after every event queued before it.
//!
//! [`InputSink::inject`]: crate::platform::InputSink::inject
//! [`InputSink::inject_release`]: crate::platform::InputSink::inject_release

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use qemu_display::{KeyboardModifiers, KeyboardProxy, MouseButton, MouseProxy};
use tracing::{debug, warn};

use super::listener::Shared;
use super::qnums::QNUMS;
use crate::platform::{Button, EvdevCode, Gone, InputEvent, LockState, Size};

/// How many events [`InputSink::inject`] lets wait to be sent.
///
/// [`InputSink::inject`]: crate::platform::InputSink::inject
pub const INPUT_QUEUE_LIMIT: usize = 8192;

/// How many more events [`InputSink::inject_release`] may queue beyond
/// [`INPUT_QUEUE_LIMIT`]. Ample for every key and button held at once.
///
/// [`InputSink::inject_release`]: crate::platform::InputSink::inject_release
pub const RELEASE_RESERVE: usize = 512;

/// The longest andris trusts what it last read of qemu's `IsAbsolute`.
pub const ABSOLUTE_MAX_AGE: Duration = Duration::from_secs(1);

/// The qnum qemu's keyboard takes for evdev `code`, or `None` if the key
/// has none.
pub fn qnum(code: EvdevCode) -> Option<u32> {
    let index = QNUMS.binary_search_by_key(&code.0, |&(c, _)| c).ok()?;
    QNUMS.get(index).map(|&(_, qnum)| u32::from(qnum))
}

/// qemu's number for `button`.
fn mouse_button(button: Button) -> MouseButton {
    match button {
        Button::Left => MouseButton::Left,
        Button::Middle => MouseButton::Middle,
        Button::Right => MouseButton::Right,
        Button::WheelUp => MouseButton::WheelUp,
        Button::WheelDown => MouseButton::WheelDown,
        Button::Side => MouseButton::Side,
        Button::Extra => MouseButton::Extra,
    }
}

/// The lock state of a keyboard whose `Modifiers` property holds each
/// modifier for which `has` is true.
fn lock_state(has: impl Fn(KeyboardModifiers) -> bool) -> LockState {
    LockState {
        caps: has(KeyboardModifiers::Caps),
        num: has(KeyboardModifiers::Num),
        scroll: has(KeyboardModifiers::Scroll),
    }
}

/// What one pointer event becomes for qemu's mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    /// `SetAbsPosition`.
    To { x: u32, y: u32 },
    /// `RelMotion`.
    By { dx: i32, dy: i32 },
    /// Nothing to send.
    Nothing,
}

/// Where andris last put the guest's pointer, which turns positions into
/// offsets and back.
#[derive(Debug, Default)]
struct Pointer {
    at: Option<(u32, u32)>,
}

impl Pointer {
    /// A move to (`x`, `y`) on a screen of `size`, for an absolute mouse
    /// or a relative one.
    fn to(&mut self, x: u32, y: u32, absolute: bool, size: Size) -> Motion {
        let to = (
            x.min(size.width.saturating_sub(1)),
            y.min(size.height.saturating_sub(1)),
        );
        let from = self.at.replace(to);
        match (absolute, from) {
            (true, _) => Motion::To { x: to.0, y: to.1 },
            (false, Some(from)) => Motion::By {
                dx: difference(from.0, to.0),
                dy: difference(from.1, to.1),
            },
            (false, None) => Motion::Nothing,
        }
    }

    /// A move by (`dx`, `dy`) on a screen of `size`, for an absolute mouse
    /// or a relative one. An absolute mouse not yet placed starts from the
    /// middle of the screen.
    fn by(&mut self, dx: i32, dy: i32, absolute: bool, size: Size) -> Motion {
        let moved = |(x, y): (u32, u32)| {
            (
                offset(x, dx, size.width.saturating_sub(1)),
                offset(y, dy, size.height.saturating_sub(1)),
            )
        };
        if absolute {
            let to = moved(self.at.unwrap_or((size.width / 2, size.height / 2)));
            self.at = Some(to);
            Motion::To { x: to.0, y: to.1 }
        } else {
            self.at = self.at.map(moved);
            Motion::By { dx, dy }
        }
    }
}

/// `to - from`. Both are within a screen, whose sides fit an `i32`.
fn difference(from: u32, to: u32) -> i32 {
    let difference = i64::from(to) - i64::from(from);
    difference.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// `from` moved by `by`, kept within 0 to `max`.
fn offset(from: u32, by: i32, max: u32) -> u32 {
    (i64::from(from) + i64::from(by)).clamp(0, i64::from(max)) as u32
}

/// qemu's keyboard and mouse on the console andris shows.
pub struct Devices {
    pub keyboard: KeyboardProxy<'static>,
    pub mouse: MouseProxy<'static>,
}

/// A request to the input thread.
enum InputRequest {
    /// Send an event; there is no answer.
    Event(InputEvent),
    /// Read the lock state, after every event queued before this.
    LockState(SyncSender<Result<LockState>>),
}

/// The sending end of the input thread's queue: one FIFO, bounded by
/// counting the events in it, so that a release cannot overtake its own
/// press. Lock-state reads are not counted: each caller waits for its
/// answer.
pub struct InputQueue {
    requests: Sender<InputRequest>,
    queued: Arc<AtomicUsize>,
}

impl InputQueue {
    /// Start the input thread, sending to `devices`, with the screen's
    /// size from `shared`.
    pub fn start(devices: Devices, shared: Arc<Shared>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("starting the qemu input thread's runtime")?;
        let (requests, receiver) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let mut injector = Injector {
            devices,
            shared,
            pointer: Pointer::default(),
            absolute: None,
            last_failure: None,
        };
        let thread_queued = queued.clone();
        std::thread::Builder::new()
            .name("andris-qemu-input".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    match request {
                        InputRequest::Event(event) => {
                            thread_queued.fetch_sub(1, Ordering::AcqRel);
                            let result = runtime.block_on(injector.send(event));
                            injector.report(event, result);
                        }
                        InputRequest::LockState(reply) => {
                            let _ = reply.send(runtime.block_on(injector.lock_state()));
                        }
                    }
                }
            })
            .context("starting the qemu input thread")?;
        Ok(Self { requests, queued })
    }

    /// Queue `event`, unless `limit` events are already queued.
    pub fn push(&self, event: InputEvent, limit: usize) -> Result<()> {
        let mut queued = self.queued.load(Ordering::Acquire);
        loop {
            if queued >= limit {
                bail!("the qemu input queue is full: {limit} events are waiting to be sent");
            }
            match self.queued.compare_exchange_weak(
                queued,
                queued + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => queued = now,
            }
        }
        if self.requests.send(InputRequest::Event(event)).is_err() {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(stopped());
        }
        Ok(())
    }

    /// The guest's lock state, read after every event already queued.
    pub fn lock_state(&self) -> Result<LockState> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.requests
            .send(InputRequest::LockState(reply))
            .map_err(|_| stopped())?;
        answer.recv().map_err(|_| stopped())?
    }
}

/// The error once the input thread has stopped.
fn stopped() -> anyhow::Error {
    anyhow::Error::new(Gone(
        "the qemu input thread has stopped, so the guest takes no more input".to_string(),
    ))
}

/// The input thread's state: the devices, and what it knows of the
/// pointer.
struct Injector {
    devices: Devices,
    shared: Arc<Shared>,
    pointer: Pointer,
    /// What qemu last said of `IsAbsolute`, and when.
    absolute: Option<(bool, Instant)>,
    /// The last failure logged, so that a run of one is logged once.
    last_failure: Option<String>,
}

impl Injector {
    /// Send `event` to qemu, and wait for its reply.
    async fn send(&mut self, event: InputEvent) -> Result<()> {
        match event {
            InputEvent::Key { code, pressed } => {
                let Some(qnum) = qnum(code) else {
                    debug!(code = code.0, "evdev key {} has no qnum; dropped", code.0);
                    return Ok(());
                };
                let keyboard = &self.devices.keyboard;
                if pressed {
                    keyboard.press(qnum).await
                } else {
                    keyboard.release(qnum).await
                }
                .with_context(|| format!("sending qemu qnum {qnum:#04x}"))
            }
            InputEvent::Button { button, pressed } => {
                let mouse = &self.devices.mouse;
                let qemu = mouse_button(button);
                if pressed {
                    mouse.press(qemu).await
                } else {
                    mouse.release(qemu).await
                }
                .with_context(|| format!("sending qemu the {button:?} button"))
            }
            InputEvent::PointerAbsolute { x, y } => {
                let (absolute, size) = (self.is_absolute().await?, self.shared.size()?);
                let motion = self.pointer.to(x, y, absolute, size);
                self.motion(motion).await
            }
            InputEvent::PointerRelative { dx, dy } => {
                let (absolute, size) = (self.is_absolute().await?, self.shared.size()?);
                let motion = self.pointer.by(dx, dy, absolute, size);
                self.motion(motion).await
            }
        }
    }

    /// Whether qemu's guest mouse is absolute, read afresh if what andris
    /// knows is older than [`ABSOLUTE_MAX_AGE`].
    async fn is_absolute(&mut self) -> Result<bool> {
        if let Some((absolute, read)) = self.absolute
            && read.elapsed() < ABSOLUTE_MAX_AGE
        {
            return Ok(absolute);
        }
        let absolute = self
            .devices
            .mouse
            .is_absolute()
            .await
            .context("asking qemu whether the guest's mouse is absolute")?;
        self.absolute = Some((absolute, Instant::now()));
        Ok(absolute)
    }

    /// Send `motion`. A refusal, as when the guest has changed mouse,
    /// makes the next motion read `IsAbsolute` again.
    async fn motion(&mut self, motion: Motion) -> Result<()> {
        let mouse = &self.devices.mouse;
        let result = match motion {
            Motion::To { x, y } => mouse
                .set_abs_position(x, y)
                .await
                .with_context(|| format!("moving the guest's pointer to ({x}, {y})")),
            Motion::By { dx, dy } => mouse
                .rel_motion(dx, dy)
                .await
                .with_context(|| format!("moving the guest's pointer by ({dx}, {dy})")),
            Motion::Nothing => Ok(()),
        };
        if result.is_err() {
            self.absolute = None;
        }
        result
    }

    /// The keyboard's lock state.
    async fn lock_state(&self) -> Result<LockState> {
        self.devices
            .keyboard
            .modifiers()
            .await
            .map(|modifiers| lock_state(|modifier| modifiers.contains(modifier)))
            .context("reading the guest's lock state from qemu")
    }

    /// Log `result`'s failure as a warning, unless it is the one logged
    /// last; a success clears that.
    fn report(&mut self, event: InputEvent, result: Result<()>) {
        match result {
            Ok(()) => self.last_failure = None,
            Err(error) => {
                let message = format!("{error:#}");
                if self.last_failure.as_deref() != Some(message.as_str()) {
                    warn!(
                        ?event,
                        "qemu did not take an input event, which is dropped: {message}"
                    );
                    self.last_failure = Some(message);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Size = Size {
        width: 800,
        height: 600,
    };

    #[test]
    fn keys_map_to_their_qnums() {
        // One-byte set 1 codes are their own qnums.
        assert_eq!(qnum(EvdevCode(1)), Some(0x01)); // KEY_ESC
        assert_eq!(qnum(EvdevCode(30)), Some(0x1e)); // KEY_A
        assert_eq!(qnum(EvdevCode(58)), Some(0x3a)); // KEY_CAPSLOCK
        assert_eq!(qnum(EvdevCode(88)), Some(0x58)); // KEY_F12
        // 0xe0-prefixed codes fold into the top bit.
        assert_eq!(qnum(EvdevCode(96)), Some(0x9c)); // KEY_KPENTER, e0 1c
        assert_eq!(qnum(EvdevCode(97)), Some(0x9d)); // KEY_RIGHTCTRL, e0 1d
        assert_eq!(qnum(EvdevCode(103)), Some(0xc8)); // KEY_UP, e0 48
        assert_eq!(qnum(EvdevCode(125)), Some(0xdb)); // KEY_LEFTMETA, e0 5b
        // SysRq is 0x54, and Pause folds its e1 sequence to e0 46.
        assert_eq!(qnum(EvdevCode(99)), Some(0x54)); // KEY_SYSRQ
        assert_eq!(qnum(EvdevCode(119)), Some(0xc6)); // KEY_PAUSE
        // The Hanja and Hangeul keys lose their top bit.
        assert_eq!(qnum(EvdevCode(122)), Some(0x72)); // KEY_HANGEUL
        assert_eq!(qnum(EvdevCode(123)), Some(0x71)); // KEY_HANJA
        // No key, and keys with no set 1 code.
        assert_eq!(qnum(EvdevCode(0)), None);
        assert_eq!(qnum(EvdevCode(240)), None);
        assert_eq!(qnum(EvdevCode(u16::MAX)), None);
    }

    #[test]
    fn the_table_is_sorted_and_its_qnums_distinct() {
        assert!(QNUMS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(QNUMS.iter().all(|&(_, qnum)| qnum != 0 && qnum < 0x100));
        // Only SysRq has two codes: keycodemapdb gives the unnamed evdev
        // code 84 its scan code too.
        let mut qnums: Vec<(u16, u16)> = QNUMS.iter().map(|&(code, qnum)| (qnum, code)).collect();
        qnums.sort_unstable();
        let shared: Vec<_> = qnums
            .windows(2)
            .filter(|pair| pair[0].0 == pair[1].0)
            .map(|pair| (pair[0], pair[1]))
            .collect();
        assert_eq!(shared, [((0x54, 84), (0x54, 99))]);
    }

    #[test]
    fn the_queue_keeps_room_for_releases() {
        let (requests, receiver) = mpsc::channel();
        let queue = InputQueue {
            requests,
            queued: Arc::new(AtomicUsize::new(0)),
        };
        let key = |pressed| InputEvent::Key {
            code: EvdevCode(30),
            pressed,
        };
        for _ in 0..INPUT_QUEUE_LIMIT {
            queue.push(key(true), INPUT_QUEUE_LIMIT).unwrap();
        }
        let full = queue.push(key(true), INPUT_QUEUE_LIMIT).unwrap_err();
        assert!(full.to_string().contains("full"), "{full}");
        for _ in 0..RELEASE_RESERVE {
            queue
                .push(key(false), INPUT_QUEUE_LIMIT + RELEASE_RESERVE)
                .unwrap();
        }
        assert!(
            queue
                .push(key(false), INPUT_QUEUE_LIMIT + RELEASE_RESERVE)
                .is_err()
        );
        // Once the thread has gone, pushing fails as gone, and the count
        // is put back.
        drop(receiver);
        queue.queued.store(0, Ordering::Release);
        let error = queue.push(key(true), INPUT_QUEUE_LIMIT).unwrap_err();
        assert!(crate::platform::is_gone(&error));
        assert_eq!(queue.queued.load(Ordering::Acquire), 0);
        assert!(crate::platform::is_gone(&queue.lock_state().unwrap_err()));
    }

    #[test]
    fn buttons_keep_qemus_numbering() {
        let buttons = [
            (Button::Left, 0),
            (Button::Middle, 1),
            (Button::Right, 2),
            (Button::WheelUp, 3),
            (Button::WheelDown, 4),
            (Button::Side, 5),
            (Button::Extra, 6),
        ];
        for (button, number) in buttons {
            assert_eq!(mouse_button(button) as u32, number, "{button:?}");
        }
    }

    #[test]
    fn modifiers_become_locks() {
        let modifiers = KeyboardModifiers::Caps | KeyboardModifiers::Scroll;
        assert_eq!(
            lock_state(|modifier| modifiers.contains(modifier)),
            LockState {
                caps: true,
                num: false,
                scroll: true,
            }
        );
        assert_eq!(
            lock_state(|modifier| modifier == KeyboardModifiers::Num),
            LockState {
                caps: false,
                num: true,
                scroll: false,
            }
        );
        assert_eq!(lock_state(|_| false), LockState::default());
    }

    #[test]
    fn an_absolute_mouse_takes_positions_and_tracks_offsets() {
        let mut pointer = Pointer::default();
        assert_eq!(
            pointer.to(10, 20, true, SCREEN),
            Motion::To { x: 10, y: 20 }
        );
        assert_eq!(
            pointer.to(900, 700, true, SCREEN),
            Motion::To { x: 799, y: 599 }
        );
        assert_eq!(
            pointer.by(-100, -1000, true, SCREEN),
            Motion::To { x: 699, y: 0 }
        );
        assert_eq!(pointer.by(5, 7, true, SCREEN), Motion::To { x: 704, y: 7 });
        // An offset before any position starts from the middle.
        assert_eq!(
            Pointer::default().by(1, -1, true, SCREEN),
            Motion::To { x: 401, y: 299 }
        );
    }

    #[test]
    fn a_relative_mouse_takes_offsets_and_differences_of_positions() {
        let mut pointer = Pointer::default();
        assert_eq!(pointer.to(100, 100, false, SCREEN), Motion::Nothing);
        assert_eq!(
            pointer.to(90, 130, false, SCREEN),
            Motion::By { dx: -10, dy: 30 }
        );
        assert_eq!(
            pointer.by(-3, 4, false, SCREEN),
            Motion::By { dx: -3, dy: 4 }
        );
        // The offset moved the pointer andris tracks, clamped to the screen.
        assert_eq!(
            pointer.to(87, 134, false, SCREEN),
            Motion::By { dx: 0, dy: 0 }
        );
        assert_eq!(
            pointer.by(-1000, 0, false, SCREEN),
            Motion::By { dx: -1000, dy: 0 }
        );
        assert_eq!(
            pointer.to(5, 134, false, SCREEN),
            Motion::By { dx: 5, dy: 0 }
        );
        // An offset before any position is sent, and places nothing.
        let mut fresh = Pointer::default();
        assert_eq!(fresh.by(2, 2, false, SCREEN), Motion::By { dx: 2, dy: 2 });
        assert_eq!(fresh.to(1, 1, false, SCREEN), Motion::Nothing);
    }

    #[test]
    fn differences_and_offsets_stay_in_range() {
        assert_eq!(difference(u32::MAX, 0), i32::MIN);
        assert_eq!(difference(0, u32::MAX), i32::MAX);
        assert_eq!(offset(0, i32::MIN, 10), 0);
        assert_eq!(offset(10, i32::MAX, 10), 10);
        assert_eq!(offset(5, -2, 10), 3);
    }
}
