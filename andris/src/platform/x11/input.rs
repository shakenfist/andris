//! The input thread: injects events with XTest, and reads the lock state
//! from XKB's indicators, in the order they were queued.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};
use x11rb::connection::Connection;
use x11rb::errors::ConnectionError;
use x11rb::protocol::xproto::{self, Atom, Window};
use x11rb::protocol::{Event, xkb, xtest};
use x11rb::rust_connection::RustConnection;

use super::keymap::{self, KeyName, Keymap, Unmapped};
use super::{Fault, answer, connection_failed, lost, stopped};
use crate::platform::{Button, GoneSender, InputEvent, LockState};

/// A request to the input thread.
pub(super) enum InputRequest {
    /// Inject an event; there is no answer.
    Event(InputEvent),
    /// Read the lock state, after every event queued before this.
    LockState(SyncSender<Result<LockState>>),
}

/// The sending end of the input thread's queue.
///
/// One unbounded FIFO, bounded instead by counting the events in it: an
/// event is counted when queued and uncounted when the input thread takes
/// it. [`InputQueue::push`] refuses an event once `limit` are counted, so
/// [`InputSink::inject`] and [`InputSink::inject_release`] differ only in
/// the limit they pass. A second queue for releases, drained first, would
/// let a release overtake its own press still waiting here, leaving the
/// key held. Lock-state reads are not counted: each caller blocks for its
/// answer, so there are never more than there are callers.
pub(super) struct InputQueue {
    pub(super) requests: Sender<InputRequest>,
    queued: Arc<AtomicUsize>,
}

/// The input thread's end of an [`InputQueue`].
pub(super) struct InputReceiver {
    requests: Receiver<InputRequest>,
    queued: Arc<AtomicUsize>,
}

impl InputQueue {
    pub(super) fn new() -> (Self, InputReceiver) {
        let (requests, receiver) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        (
            Self {
                requests,
                queued: queued.clone(),
            },
            InputReceiver {
                requests: receiver,
                queued,
            },
        )
    }

    /// Queue `event`, unless `limit` events are already queued.
    pub(super) fn push(&self, event: InputEvent, limit: usize) -> Result<()> {
        let mut queued = self.queued.load(Ordering::Acquire);
        loop {
            if queued >= limit {
                bail!("the X11 input queue is full: {limit} events are waiting to be injected");
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
            return Err(stopped("input"));
        }
        Ok(())
    }
}

impl InputReceiver {
    /// Uncount `request` if it is an event.
    fn taken(&self, request: InputRequest) -> InputRequest {
        if matches!(request, InputRequest::Event(_)) {
            self.queued.fetch_sub(1, Ordering::AcqRel);
        }
        request
    }

    /// The next request, waiting for one; `None` once the queue closes.
    fn recv(&self) -> Option<InputRequest> {
        self.requests.recv().ok().map(|request| self.taken(request))
    }

    /// The next request, if one is waiting.
    fn try_recv(&self) -> Option<InputRequest> {
        self.requests
            .try_recv()
            .ok()
            .map(|request| self.taken(request))
    }
}

/// The XKB indicators that show the lock keys, as masks of the bits in
/// `GetIndicatorState`'s `state`. `None` if the server has no indicator
/// by that name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LockIndicators {
    caps: Option<u32>,
    num: Option<u32>,
    scroll: Option<u32>,
}

/// The indicator names andris looks for, in [`LockIndicators`] order.
const LOCK_INDICATOR_NAMES: [&str; 3] = ["Caps Lock", "Num Lock", "Scroll Lock"];

impl LockIndicators {
    /// Find the lock indicators. `named` is `GetNames`' `indicators`: bit
    /// `i` is set if indicator `i` has a name, and `names` holds those
    /// names in order of `i`. `wanted` holds the atoms of
    /// [`LOCK_INDICATOR_NAMES`], 0 for one the server has never interned.
    fn find(named: u32, names: &[Atom], wanted: [Atom; 3]) -> Self {
        let bits = (0..32).filter(|bit| named & (1 << bit) != 0);
        let mask_of = |atom: Atom| {
            if atom == 0 {
                return None;
            }
            bits.clone()
                .zip(names)
                .find(|&(_, &name)| name == atom)
                .map(|(bit, _)| 1u32 << bit)
        };
        Self {
            caps: mask_of(wanted[0]),
            num: mask_of(wanted[1]),
            scroll: mask_of(wanted[2]),
        }
    }

    /// The lock state `state` shows; a missing indicator reads as off.
    fn read(&self, state: u32) -> LockState {
        let on = |mask: Option<u32>| mask.is_some_and(|mask| state & mask != 0);
        LockState {
            caps: on(self.caps),
            num: on(self.num),
            scroll: on(self.scroll),
        }
    }
}

/// The input thread's state: its connection and what it resolved at
/// startup.
pub(super) struct Injector {
    conn: RustConnection,
    root: Window,
    keymap: Keymap,
    locks: LockIndicators,
    /// Evdev codes already logged as having no key here.
    unmapped: HashSet<u16>,
    /// Whether an X error for injected input has been logged.
    warned_x_error: bool,
}

impl Injector {
    /// Read the server's key names, aliases and indicator names over
    /// `conn`, which has made the XKB handshake, and resolve the keymap
    /// and lock indicators from them.
    pub(super) fn new(conn: RustConnection, root: Window) -> Result<Self> {
        let which = xkb::NameDetail::KEY_NAMES
            | xkb::NameDetail::KEY_ALIASES
            | xkb::NameDetail::INDICATOR_NAMES;
        let names = xkb::get_names(&conn, xkb::ID::USE_CORE_KBD.into(), which)?
            .reply()
            .context("reading the X server's XKB key and indicator names")?;
        let values = names.value_list;
        let key_names: Vec<KeyName> = values
            .key_names
            .unwrap_or_default()
            .into_iter()
            .map(|key| key.name)
            .collect();
        let aliases: Vec<(KeyName, KeyName)> = values
            .key_aliases
            .unwrap_or_default()
            .into_iter()
            .map(|alias| (alias.alias, alias.real))
            .collect();
        let keymap = Keymap::new(names.first_key, &key_names, &aliases);

        let mut wanted = [0; 3];
        for (atom, name) in wanted.iter_mut().zip(LOCK_INDICATOR_NAMES) {
            *atom = xproto::intern_atom(&conn, true, name.as_bytes())?
                .reply()
                .with_context(|| format!("looking up the atom {name:?}"))?
                .atom;
        }
        let locks = LockIndicators::find(
            names.indicators,
            &values.indicator_names.unwrap_or_default(),
            wanted,
        );
        for (mask, name) in [locks.caps, locks.num, locks.scroll]
            .into_iter()
            .zip(LOCK_INDICATOR_NAMES)
        {
            if mask.is_none() {
                warn!(
                    indicator = name,
                    "the X server has no XKB indicator by this name; it will always read as off"
                );
            }
        }
        debug!(
            keys = key_names.iter().filter(|name| **name != [0; 4]).count(),
            aliases = aliases.len(),
            "read the X server's key names"
        );
        Ok(Self {
            conn,
            root,
            keymap,
            locks,
            unmapped: HashSet::new(),
            warned_x_error: false,
        })
    }

    /// Send the XTest request for `event`. A key this server has no
    /// keycode for is logged, once per key, and dropped.
    fn inject(&mut self, event: InputEvent) -> Result<(), ConnectionError> {
        let (kind, detail, root, x, y) = match event {
            InputEvent::Key { code, pressed } => {
                let keycode = match self.keymap.keycode(code) {
                    Ok(keycode) => keycode,
                    Err(why) => {
                        if self.unmapped.insert(code.0) {
                            let reason = match why {
                                Unmapped::NoName => {
                                    "xkeyboard-config's evdev keycodes name no such key".to_string()
                                }
                                Unmapped::NotOnServer(name) => format!(
                                    "the X server has no key named {}",
                                    keymap::display_name(&name)
                                ),
                            };
                            warn!(evdev_code = code.0, %reason, "dropping a key the X server lacks");
                        }
                        return Ok(());
                    }
                };
                let kind = if pressed {
                    xproto::KEY_PRESS_EVENT
                } else {
                    xproto::KEY_RELEASE_EVENT
                };
                (kind, keycode, x11rb::NONE, 0, 0)
            }
            // For motion, a detail of 0 means absolute, to (x, y) on the
            // given root, and 1 means relative, by (x, y).
            InputEvent::PointerAbsolute { x, y } => (
                xproto::MOTION_NOTIFY_EVENT,
                0,
                self.root,
                coordinate(x),
                coordinate(y),
            ),
            InputEvent::PointerRelative { dx, dy } => (
                xproto::MOTION_NOTIFY_EVENT,
                1,
                x11rb::NONE,
                offset(dx),
                offset(dy),
            ),
            InputEvent::Button { button, pressed } => {
                let kind = if pressed {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                };
                (kind, x_button(button), x11rb::NONE, 0, 0)
            }
        };
        // A time of CurrentTime applies the event at once; device id 0
        // because these are core events.
        xtest::fake_input(&self.conn, kind, detail, x11rb::CURRENT_TIME, root, x, y, 0)?;
        Ok(())
    }

    /// The lock state now, after every event already sent.
    fn lock_state(&self) -> Result<LockState, Fault> {
        let state = xkb::get_indicator_state(&self.conn, xkb::ID::USE_CORE_KBD.into())?.reply()?;
        Ok(self.locks.read(state.state))
    }

    /// Flush a batch of requests, and log any error the server has
    /// returned for earlier ones, once.
    fn finish_batch(&mut self) -> Result<(), ConnectionError> {
        self.conn.flush()?;
        while let Some(event) = self.conn.poll_for_event()? {
            if let Event::Error(e) = event
                && !self.warned_x_error
            {
                self.warned_x_error = true;
                warn!(
                    error = ?e.error_kind,
                    major_opcode = e.major_opcode,
                    minor_opcode = e.minor_opcode,
                    "the X server refused injected input; later refusals are not logged"
                );
            }
        }
        Ok(())
    }
}

/// An X coordinate for desktop position `v`; X positions are 16 bit.
fn coordinate(v: u32) -> i16 {
    i16::try_from(v).unwrap_or(i16::MAX)
}

/// An X motion offset for `v`, saturated to 16 bits.
fn offset(v: i32) -> i16 {
    v.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

/// The X button number for `button`.
fn x_button(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
        Button::WheelUp => 4,
        Button::WheelDown => 5,
        Button::Side => 8,
        Button::Extra => 9,
    }
}

/// Serve input requests until the queue closes or the connection fails,
/// flushing after each batch: every request already queued when the
/// thread wakes. Sets `gone` if the connection fails.
pub(super) fn serve_input(mut injector: Injector, requests: InputReceiver, gone: GoneSender) {
    while let Some(first) = requests.recv() {
        let mut next = Some(first);
        while let Some(request) = next {
            let failed = match request {
                InputRequest::Event(event) => injector.inject(event).err().map(connection_failed),
                InputRequest::LockState(reply) => answer(reply, injector.lock_state()),
            };
            if let Some(e) = failed {
                lost(&gone, "input", "input has stopped", &e);
                return;
            }
            next = requests.try_recv();
        }
        if let Err(e) = injector.finish_batch() {
            lost(&gone, "input", "input has stopped", &e);
            return;
        }
    }
    debug!("the X11 input thread is stopping");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::x11::{INPUT_QUEUE_LIMIT, RELEASE_RESERVE};

    const CAPS: Atom = 100;
    const NUM: Atom = 101;
    const SCROLL: Atom = 102;
    const OTHER: Atom = 200;

    #[test]
    fn lock_indicators_are_found_by_name() {
        // Indicators 0, 1, 2 and 5 are named: caps, num, other, scroll.
        let locks =
            LockIndicators::find(0b10_0111, &[CAPS, NUM, OTHER, SCROLL], [CAPS, NUM, SCROLL]);
        assert_eq!(
            locks,
            LockIndicators {
                caps: Some(1),
                num: Some(2),
                scroll: Some(1 << 5),
            }
        );
        assert_eq!(
            locks.read(0b10_0001),
            LockState {
                caps: true,
                num: false,
                scroll: true
            }
        );
        // An indicator that is lit but not a lock changes nothing.
        assert_eq!(locks.read(0b100), LockState::default());
    }

    #[test]
    fn missing_lock_indicators_read_as_off() {
        // The server never interned "Scroll Lock" (atom 0), and has no
        // indicator named "Num Lock".
        let locks = LockIndicators::find(0b1, &[CAPS], [CAPS, NUM, 0]);
        assert_eq!(locks.num, None);
        assert_eq!(locks.scroll, None);
        assert_eq!(
            locks.read(u32::MAX),
            LockState {
                caps: true,
                num: false,
                scroll: false
            }
        );
        // A name of atom 0 never matches a missing atom.
        let locks = LockIndicators::find(0b1, &[0], [0, 0, 0]);
        assert_eq!(locks, LockIndicators::default());
    }

    #[test]
    fn coordinates_and_offsets_saturate() {
        assert_eq!(coordinate(5), 5);
        assert_eq!(coordinate(40_000), i16::MAX);
        assert_eq!(offset(-5), -5);
        assert_eq!(offset(-40_000), i16::MIN);
        assert_eq!(offset(40_000), i16::MAX);
    }

    #[test]
    fn buttons_use_the_x_numbering() {
        let numbers: Vec<u8> = [
            Button::Left,
            Button::Middle,
            Button::Right,
            Button::WheelUp,
            Button::WheelDown,
            Button::Side,
            Button::Extra,
        ]
        .into_iter()
        .map(x_button)
        .collect();
        assert_eq!(numbers, [1, 2, 3, 4, 5, 8, 9]);
    }

    fn key(code: u16, pressed: bool) -> InputEvent {
        InputEvent::Key {
            code: crate::platform::EvdevCode(code),
            pressed,
        }
    }

    /// Every event waiting in `queue`, in order.
    fn drain(queue: &InputReceiver) -> Vec<InputEvent> {
        std::iter::from_fn(|| queue.try_recv())
            .filter_map(|request| match request {
                InputRequest::Event(event) => Some(event),
                InputRequest::LockState(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_full_input_queue_refuses_events_but_keeps_room_for_releases() {
        let (queue, receiver) = InputQueue::new();
        // A key held, then a flood that fills the queue.
        queue.push(key(30, true), INPUT_QUEUE_LIMIT).unwrap();
        for _ in 1..INPUT_QUEUE_LIMIT {
            queue
                .push(
                    InputEvent::PointerRelative { dx: 1, dy: 0 },
                    INPUT_QUEUE_LIMIT,
                )
                .unwrap();
        }
        let error = queue
            .push(key(31, true), INPUT_QUEUE_LIMIT)
            .unwrap_err()
            .to_string();
        assert!(error.contains("input queue is full"), "{error}");
        // A client's own release is refused like any other event.
        assert!(queue.push(key(30, false), INPUT_QUEUE_LIMIT).is_err());

        // The release andris owes still gets through, after the press.
        let platform_limit = INPUT_QUEUE_LIMIT + RELEASE_RESERVE;
        queue.push(key(30, false), platform_limit).unwrap();
        let events = drain(&receiver);
        assert_eq!(events.len(), INPUT_QUEUE_LIMIT + 1);
        assert_eq!(events.first(), Some(&key(30, true)));
        assert_eq!(events.last(), Some(&key(30, false)));

        // Drained, the queue takes events again.
        queue.push(key(31, true), INPUT_QUEUE_LIMIT).unwrap();
        assert_eq!(drain(&receiver), vec![key(31, true)]);
    }

    #[test]
    fn the_release_reserve_is_bounded_too() {
        let (queue, _receiver) = InputQueue::new();
        let limit = INPUT_QUEUE_LIMIT + RELEASE_RESERVE;
        for _ in 0..limit {
            queue.push(key(30, false), limit).unwrap();
        }
        assert!(queue.push(key(30, false), limit).is_err());
    }

    #[test]
    fn a_stopped_input_thread_uncounts_the_event() {
        let (queue, receiver) = InputQueue::new();
        drop(receiver);
        let error = queue.push(key(30, true), 1).unwrap_err().to_string();
        assert!(error.contains("input thread has stopped"), "{error}");
        assert_eq!(queue.queued.load(Ordering::Acquire), 0);
    }
}
