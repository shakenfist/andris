//! The clipboard thread: follows the `CLIPBOARD` selection, fetching the
//! text of each program that takes it, and takes it itself to serve the
//! text andris is given.
//!
//! # Following the desktop's clipboard
//!
//! XFixes reports each new owner of `CLIPBOARD`. For an owner other than
//! andris, the thread asks for `TARGETS`, then for `UTF8_STRING` if the
//! owner offers it, or else `STRING`, which is Latin-1. An owner that
//! refuses `TARGETS` is asked for `UTF8_STRING` and then `STRING` anyway.
//! Each answer arrives in a property of the thread's own window, whole or
//! in pieces (INCR). The text, at most [`MAX_CLIPBOARD_TEXT`] bytes, is
//! published on a `watch` channel; a copy of the same text is published
//! again. When the selection has no owner, or the owner has no text, or
//! its text is too large, `None` is published.
//!
//! Every conversion the thread asks for has [`CONVERSION_TIMEOUT`] to
//! answer, renewed by each piece of an INCR transfer. One that does not
//! answer in time publishes `None`: the text published before belonged to
//! an earlier owner, and is no longer on the clipboard. xrdp's chansrv
//! keeps `CLIPBOARD` after the RDP client disconnects, and conversions it
//! is asked for then never answer, so this is a case the thread meets.
//!
//! # Serving andris's text
//!
//! Text queued with [`ClipboardHandle::set`] is put on the clipboard by
//! taking `CLIPBOARD` with a timestamp from the X server, which a
//! zero-length property append on the thread's window yields. While it
//! owns the selection, the thread answers `TARGETS`, `TIMESTAMP`,
//! `UTF8_STRING`, `text/plain;charset=utf-8`, and `STRING` when the text
//! is all Latin-1; it refuses every other target, `MULTIPLE` included.
//! Text over [`INCR_CHUNK`] bytes, or over what one request can carry if
//! that is less, goes in pieces (INCR), one per property deletion by the
//! requestor; each transfer has [`TRANSFER_TIMEOUT`] to take each piece,
//! and is dropped if its window is destroyed. Andris's own text is never
//! published: taking the selection publishes `None`, since the text
//! published before has left the clipboard. When another program takes
//! the selection, the thread stops serving and fetches that program's
//! text.
//!
//! # The event loop
//!
//! The thread never waits on another client: it blocks only on its X
//! connection's events, and keeps per-conversion and per-transfer state
//! with deadlines. A timer thread wakes it when the earliest deadline
//! passes, and when text is queued, by sending a `ClientMessage` to its
//! window; only the platform's drop sends one carrying the wake cookie,
//! which stops it.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::watch;
use tracing::{debug, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, EventMask,
    GetPropertyReply, PropMode, Property, SelectionClearEvent, SelectionNotifyEvent,
    SelectionRequestEvent, Timestamp, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::{Fault, connection_failed, create_wake_window, is_wake, lost, stopped, x_error};
use crate::platform::{ClipboardReceiver, GoneSender, MAX_CLIPBOARD_TEXT};

/// How long an owner has to answer each conversion the thread asks for,
/// and each piece of an INCR transfer to it.
pub const CONVERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a requestor has to take each piece of an INCR transfer from
/// andris before the transfer is dropped.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);

/// The most text andris writes in one property: larger text goes by INCR.
/// Well under the 16 MiB a request can carry with BIG-REQUESTS, so that
/// INCR is used for large texts on every server, as other X clients do.
pub const INCR_CHUNK: usize = 1024 * 1024;

/// A `ChangeProperty` request's size beyond its data: 24 bytes, and 4
/// more for the length field of a BIG-REQUESTS request.
const CHANGE_PROPERTY_OVERHEAD: usize = 28;

/// The most INCR transfers from andris at once. A request beyond it is
/// refused.
const MAX_TRANSFERS: usize = 64;

/// The window properties conversions arrive in, used in turn, so that a
/// late answer to one that was given up on lands apart from the next.
const SLOTS: usize = 4;

/// The atoms the thread uses beyond the predefined ones.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    utf8_string: Atom,
    text_plain: Atom,
    incr: Atom,
    /// The property andris appends nothing to, to learn the time.
    stamp: Atom,
    /// The properties conversions arrive in.
    slots: [Atom; SLOTS],
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self> {
        let names: [&str; 7 + SLOTS] = [
            "CLIPBOARD",
            "TARGETS",
            "TIMESTAMP",
            "UTF8_STRING",
            "text/plain;charset=utf-8",
            "INCR",
            "ANDRIS_CLIPBOARD_STAMP",
            "ANDRIS_CLIPBOARD_0",
            "ANDRIS_CLIPBOARD_1",
            "ANDRIS_CLIPBOARD_2",
            "ANDRIS_CLIPBOARD_3",
        ];
        let cookies = names
            .iter()
            .map(|name| xproto::intern_atom(conn, false, name.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut atoms = [NONE; 7 + SLOTS];
        for ((atom, cookie), name) in atoms.iter_mut().zip(cookies).zip(names) {
            *atom = cookie
                .reply()
                .with_context(|| format!("interning the atom {name}"))?
                .atom;
        }
        let [
            clipboard,
            targets,
            timestamp,
            utf8_string,
            text_plain,
            incr,
            stamp,
            slot0,
            slot1,
            slot2,
            slot3,
        ] = atoms;
        Ok(Self {
            clipboard,
            targets,
            timestamp,
            utf8_string,
            text_plain,
            incr,
            stamp,
            slots: [slot0, slot1, slot2, slot3],
        })
    }
}

/// The predefined `STRING` atom, Latin-1 text.
fn string_atom() -> Atom {
    AtomEnum::STRING.into()
}

/// `text` as Latin-1, or `None` if it has a character Latin-1 lacks.
fn to_latin1(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect()
}

/// Latin-1 `bytes` as text. Every byte is a character.
fn from_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

/// The targets andris offers for its text: `STRING` only if the text is
/// all Latin-1. Never `MULTIPLE`.
fn offered_targets(atoms: &Atoms, latin1: bool) -> Vec<Atom> {
    let mut targets = vec![
        atoms.targets,
        atoms.timestamp,
        atoms.utf8_string,
        atoms.text_plain,
    ];
    if latin1 {
        targets.push(string_atom());
    }
    targets
}

/// Which of `offered` to ask for, in order: `UTF8_STRING`, then `STRING`.
fn wanted_targets(atoms: &Atoms, offered: &[Atom]) -> Vec<Atom> {
    [atoms.utf8_string, string_atom()]
        .into_iter()
        .filter(|target| offered.contains(target))
        .collect()
}

/// A property of 32-bit `format` as atoms; nothing for any other format.
fn atoms_in(format: u8, bytes: &[u8]) -> Vec<Atom> {
    if format != 32 {
        return Vec::new();
    }
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_ne_bytes(*word))
        .collect()
}

/// Text in a property of type `type_`, or `None` if it is not a text type
/// andris reads. UTF-8 that does not decode is repaired, not refused.
fn decode(atoms: &Atoms, type_: Atom, bytes: &[u8]) -> Option<String> {
    if type_ == string_atom() {
        Some(from_latin1(bytes))
    } else if type_ == atoms.utf8_string || type_ == atoms.text_plain {
        Some(String::from_utf8_lossy(bytes).into_owned())
    } else {
        None
    }
}

/// The most text andris writes in one property, on a server whose largest
/// request is `max_request_bytes`.
fn chunk_size(max_request_bytes: usize) -> usize {
    max_request_bytes
        .saturating_sub(CHANGE_PROPERTY_OVERHEAD)
        .clamp(1, INCR_CHUNK)
}

/// Whether server time `a` is earlier than `b`. X timestamps are
/// milliseconds that wrap every 49.7 days, so half the range either side
/// of `b` counts as before or after it.
fn earlier(a: Timestamp, b: Timestamp) -> bool {
    a.wrapping_sub(b) >= 1 << 31
}

/// A conversion the thread has asked the selection's owner for.
#[derive(Debug)]
struct Fetch {
    /// The property of andris's window it arrives in.
    property: Atom,
    /// The time it was asked for at, which the answer carries.
    time: Timestamp,
    /// The target asked for.
    target: Atom,
    /// The targets to ask for in turn if this one is refused.
    then: Vec<Atom>,
    /// The pieces so far, once the owner has begun an INCR transfer.
    incoming: Option<Incoming>,
    /// When the owner's time to answer runs out.
    deadline: Instant,
}

impl Fetch {
    /// Whether `event` answers this conversion.
    fn answered_by(&self, event: &SelectionNotifyEvent, clipboard: Atom) -> bool {
        event.selection == clipboard
            && event.target == self.target
            && (event.property == self.property || event.property == NONE)
            && (self.time == CURRENT_TIME || event.time == CURRENT_TIME || event.time == self.time)
            && self.incoming.is_none()
    }
}

/// The pieces of an INCR transfer to andris.
#[derive(Debug, Default)]
struct Incoming {
    type_: Atom,
    format: u8,
    bytes: Vec<u8>,
}

/// The text andris owns the selection with.
struct Owned {
    /// When andris took the selection.
    since: Timestamp,
    utf8: Arc<[u8]>,
    /// The same text in Latin-1, if it can be.
    latin1: Option<Arc<[u8]>>,
}

impl Owned {
    fn new(text: &str, since: Timestamp) -> Self {
        Self {
            since,
            utf8: Arc::from(text.as_bytes()),
            latin1: to_latin1(text).map(Arc::from),
        }
    }
}

/// An INCR transfer from andris to one requestor's property.
#[derive(Debug)]
struct Transfer {
    type_: Atom,
    data: Arc<[u8]>,
    /// How many bytes have been written.
    sent: usize,
    /// When the requestor's time to take the next piece runs out.
    deadline: Instant,
}

impl Transfer {
    /// The range of the next piece of at most `chunk` bytes, which is
    /// empty once every byte has been sent.
    fn next_piece(&mut self, chunk: usize) -> Range<usize> {
        let start = self.sent.min(self.data.len());
        let end = start.saturating_add(chunk).min(self.data.len());
        self.sent = end;
        start..end
    }
}

/// The transfers whose deadline is `now` or earlier.
fn stalled(transfers: &HashMap<(Window, Atom), Transfer>, now: Instant) -> Vec<(Window, Atom)> {
    transfers
        .iter()
        .filter(|(_, transfer)| transfer.deadline <= now)
        .map(|(key, _)| *key)
        .collect()
}

/// The earliest of a conversion's deadline and the transfers'.
fn earliest(
    fetch: Option<Instant>,
    transfers: &HashMap<(Window, Atom), Transfer>,
) -> Option<Instant> {
    transfers
        .values()
        .map(|transfer| transfer.deadline)
        .chain(fetch)
        .min()
}

/// What the timer thread is told.
#[derive(Debug)]
pub(super) enum Poke {
    /// Wake the clipboard thread now: text has been queued.
    Now,
    /// Wake it at this instant, its earliest deadline, or never.
    At(Option<Instant>),
}

/// The platform's side of the clipboard thread.
pub(super) struct ClipboardHandle {
    texts: Sender<Arc<str>>,
    pokes: Sender<Poke>,
    changes: ClipboardReceiver,
    /// The thread's window, which the platform's drop wakes it through.
    pub(super) window: Window,
}

impl ClipboardHandle {
    /// A receiver of the desktop's text, which sees later changes only.
    pub(super) fn changes(&self) -> ClipboardReceiver {
        let mut receiver = self.changes.clone();
        receiver.mark_unchanged();
        receiver
    }

    /// Queue `text` for the thread to put on the clipboard, and wake it.
    /// Never blocks.
    pub(super) fn set(&self, text: Arc<str>) -> Result<()> {
        if text.len() > MAX_CLIPBOARD_TEXT {
            warn!(
                bytes = text.len(),
                "not putting text on the X clipboard: it is over the {MAX_CLIPBOARD_TEXT}-byte limit"
            );
            return Ok(());
        }
        self.texts.send(text).map_err(|_| stopped("clipboard"))?;
        // The timer stops only after the clipboard thread has, so a failure
        // here was already reported by the send above.
        let _ = self.pokes.send(Poke::Now);
        Ok(())
    }
}

/// The timer thread: wakes the clipboard thread as [`Poke`]s ask.
pub(super) struct Timer {
    conn: Arc<RustConnection>,
    window: Window,
    pokes: Receiver<Poke>,
}

/// Wake the clipboard thread, with a `ClientMessage` that is not the wake
/// cookie. A failure means the connection has gone, which the clipboard
/// thread reports.
fn tick(conn: &RustConnection, window: Window) {
    let event = ClientMessageEvent::new(32, window, AtomEnum::NONE, [0u32; 5]);
    if xproto::send_event(conn, false, window, EventMask::NO_EVENT, event).is_ok() {
        let _ = conn.flush();
    }
}

/// Run the timer until every sender of pokes has gone: the platform's,
/// on its drop, and the clipboard thread's, as it stops.
pub(super) fn serve_timer(timer: Timer) {
    let mut deadline: Option<Instant> = None;
    loop {
        let poke = match deadline {
            Some(at) => timer
                .pokes
                .recv_timeout(at.saturating_duration_since(Instant::now())),
            None => timer
                .pokes
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected),
        };
        match poke {
            Ok(Poke::Now) => tick(&timer.conn, timer.window),
            Ok(Poke::At(at)) => deadline = at,
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|at| at <= Instant::now()) {
                    // The clipboard thread sends its next deadline once it
                    // has dealt with this one.
                    deadline = None;
                    tick(&timer.conn, timer.window);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Log an X error for one request, which leaves the thread running; only a
/// failed connection is an error.
fn tolerate(result: Result<(), Fault>) -> Result<(), anyhow::Error> {
    match result {
        Ok(()) => Ok(()),
        Err(Fault::Failed(e)) => {
            warn!(
                error = format!("{e:#}"),
                "a request to the X server about the clipboard failed"
            );
            Ok(())
        }
        Err(Fault::Gone(e)) => Err(e),
    }
}

/// Whether the clipboard thread carries on after an event.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// The clipboard thread's state.
pub(super) struct ClipboardThread {
    conn: Arc<RustConnection>,
    window: Window,
    atoms: Atoms,
    /// The most text written in one property.
    chunk: usize,
    /// Who owned the selection when the thread started.
    initial_owner: Window,
    published: watch::Sender<Option<Arc<str>>>,
    texts: Receiver<Arc<str>>,
    timer: Sender<Poke>,
    gone: GoneSender,
    /// Text waiting for the time andris takes the selection at.
    acquiring: Option<Arc<str>>,
    /// The text andris serves, while it owns the selection.
    owned: Option<Owned>,
    /// INCR transfers from andris, by requestor window and property.
    transfers: HashMap<(Window, Atom), Transfer>,
    /// The conversion asked of the selection's owner, if any.
    fetch: Option<Fetch>,
    /// The slot the next conversion arrives in.
    next_slot: usize,
}

/// Everything [`start_clipboard`] sets up.
pub(super) struct Started {
    pub(super) thread: ClipboardThread,
    pub(super) handle: ClipboardHandle,
    pub(super) timer: Timer,
}

/// Set up the clipboard connection: make the XFixes handshake, create the
/// thread's window, follow `CLIPBOARD`'s owner, and learn how large a
/// request may be and who owns the selection now.
pub(super) fn start_clipboard(
    conn: Arc<RustConnection>,
    root: Window,
    gone: GoneSender,
) -> Result<Started> {
    xfixes::query_version(&*conn, 5, 0)?
        .reply()
        .context("the XFIXES QueryVersion handshake")?;
    let window = create_wake_window(&conn, root)?;
    xproto::change_window_attributes(
        &*conn,
        window,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let atoms = Atoms::intern(&conn)?;
    xfixes::select_selection_input(
        &*conn,
        window,
        atoms.clipboard,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )?;
    let chunk = chunk_size(conn.maximum_request_bytes());
    let initial_owner = xproto::get_selection_owner(&*conn, atoms.clipboard)?
        .reply()
        .context("asking who owns CLIPBOARD")?
        .owner;
    let (published, changes) = watch::channel(None);
    let (texts, queued) = mpsc::channel();
    let (pokes, poked) = mpsc::channel();
    Ok(Started {
        thread: ClipboardThread {
            conn: conn.clone(),
            window,
            atoms,
            chunk,
            initial_owner,
            published,
            texts: queued,
            timer: pokes.clone(),
            gone,
            acquiring: None,
            owned: None,
            transfers: HashMap::new(),
            fetch: None,
            next_slot: 0,
        },
        handle: ClipboardHandle {
            texts,
            pokes,
            changes,
            window,
        },
        timer: Timer {
            conn,
            window,
            pokes: poked,
        },
    })
}

/// Follow and serve the clipboard until the platform's drop wakes the
/// thread to stop, or the connection fails, which sets `gone`. The thread
/// owns the only sender of texts, so its receivers see it stop.
pub(super) fn serve_clipboard(mut thread: ClipboardThread) {
    if let Err(e) = thread.run() {
        lost(
            &thread.gone,
            "clipboard",
            "the desktop's clipboard is no longer followed or served",
            &e,
        );
    }
}

impl ClipboardThread {
    /// The event loop. Returns `Ok` when woken to stop, and the error when
    /// the connection fails.
    fn run(&mut self) -> Result<(), anyhow::Error> {
        if self.initial_owner != NONE && self.initial_owner != self.window {
            // No event gave a time to ask at.
            let asked = self.start_fetch(CURRENT_TIME);
            tolerate(asked)?;
        }
        loop {
            let kept = self.housekeeping(Instant::now());
            tolerate(kept)?;
            self.conn.flush().map_err(connection_failed)?;
            let event = self.conn.wait_for_event().map_err(connection_failed)?;
            if self.handle(event)? == Flow::Stop {
                debug!("the X11 clipboard thread is stopping");
                return Ok(());
            }
        }
    }

    /// Handle one event. An X error for one request is logged and the
    /// thread carries on; a conversion it left waiting times out.
    fn handle(&mut self, event: Event) -> Result<Flow, anyhow::Error> {
        let result = match event {
            Event::XfixesSelectionNotify(e) if e.selection == self.atoms.clipboard => {
                self.owner_changed(&e)
            }
            Event::SelectionRequest(e) => self.serve(&e),
            Event::SelectionClear(e) => {
                self.cleared(&e);
                Ok(())
            }
            Event::SelectionNotify(e) if e.requestor == self.window => self.converted(&e),
            Event::PropertyNotify(e) => self.property_changed(e.window, e.atom, e.state, e.time),
            Event::DestroyNotify(e) => {
                self.transfers.retain(|(window, _), _| *window != e.window);
                Ok(())
            }
            Event::ClientMessage(message) if message.window == self.window => {
                if is_wake(&message) {
                    return Ok(Flow::Stop);
                }
                // The timer, or another client: either way, a chance to
                // look at the deadlines and the queue.
                Ok(())
            }
            // Mostly requestor windows that went before andris was done
            // with them, which is theirs to worry about.
            Event::Error(e) => {
                debug!(
                    error = format!("{:#}", x_error(&e)),
                    "a clipboard request failed"
                );
                Ok(())
            }
            _ => Ok(()),
        };
        tolerate(result)?;
        Ok(Flow::Continue)
    }

    /// Take queued text, expire what has run out of time, and tell the
    /// timer the next deadline.
    fn housekeeping(&mut self, now: Instant) -> Result<(), Fault> {
        if let Some(text) = self.texts.try_iter().last()
            && self.acquiring.replace(text).is_none()
        {
            // Appending nothing changes nothing, but the server reports
            // the time it did it at, which andris takes the selection with.
            self.conn.change_property8(
                PropMode::APPEND,
                self.window,
                self.atoms.stamp,
                AtomEnum::INTEGER,
                &[],
            )?;
        }
        if self
            .fetch
            .as_ref()
            .is_some_and(|fetch| fetch.deadline <= now)
        {
            warn!(
                "the program that owns the X clipboard did not answer within {}s; \
                 its text is not offered",
                CONVERSION_TIMEOUT.as_secs()
            );
            self.finish(None);
        }
        for key in stalled(&self.transfers, now) {
            debug!(
                requestor = key.0,
                "dropping a clipboard transfer its requestor stopped taking"
            );
            self.transfers.remove(&key);
            self.unwatch_if_idle(key.0)?;
        }
        let next = earliest(
            self.fetch.as_ref().map(|fetch| fetch.deadline),
            &self.transfers,
        );
        // The timer stops only after this thread has.
        let _ = self.timer.send(Poke::At(next));
        Ok(())
    }

    /// Publish the desktop's text: a text always, as a copy of the same
    /// text is still a copy; `None` only if something was there.
    fn publish(&self, text: Option<Arc<str>>) {
        match text {
            Some(text) => {
                debug!(bytes = text.len(), "the X clipboard has new text");
                self.published.send_replace(Some(text));
            }
            None => {
                self.published
                    .send_if_modified(|held| held.take().is_some());
            }
        }
    }

    /// End the conversion under way, publishing `text`.
    fn finish(&mut self, text: Option<String>) {
        self.fetch = None;
        let text = text.filter(|text| {
            let fits = text.len() <= MAX_CLIPBOARD_TEXT;
            if !fits {
                warn!(
                    bytes = text.len(),
                    "not offering the X clipboard's text: it is over the \
                     {MAX_CLIPBOARD_TEXT}-byte limit"
                );
            }
            fits
        });
        self.publish(text.map(Arc::from));
    }

    /// A new owner of `CLIPBOARD`, or none.
    fn owner_changed(&mut self, event: &xfixes::SelectionNotifyEvent) -> Result<(), Fault> {
        if event.owner == self.window {
            return Ok(());
        }
        if let Some(owned) = &self.owned {
            if earlier(event.selection_timestamp, owned.since) {
                // An owner from before andris took the selection.
                return Ok(());
            }
            debug!("another program took the X clipboard from andris");
            self.owned = None;
        }
        // An answer still to come for an earlier owner no longer matters.
        self.fetch = None;
        if event.owner == NONE {
            self.publish(None);
            return Ok(());
        }
        self.start_fetch(event.timestamp)
    }

    /// Ask the selection's owner for its targets, at `time`.
    fn start_fetch(&mut self, time: Timestamp) -> Result<(), Fault> {
        let property = self.atoms.slots[self.next_slot % SLOTS];
        self.next_slot = self.next_slot.wrapping_add(1) % SLOTS;
        // The property is used afresh: whatever an earlier answer left
        // in it is not this one.
        xproto::delete_property(&*self.conn, self.window, property)?;
        self.fetch = Some(Fetch {
            property,
            time,
            target: self.atoms.targets,
            then: vec![self.atoms.utf8_string, string_atom()],
            incoming: None,
            deadline: Instant::now(),
        });
        self.ask()
    }

    /// Ask for the conversion's current target, and start its clock.
    fn ask(&mut self) -> Result<(), Fault> {
        let Some(fetch) = &mut self.fetch else {
            return Ok(());
        };
        fetch.deadline = Instant::now() + CONVERSION_TIMEOUT;
        xproto::convert_selection(
            &*self.conn,
            self.window,
            self.atoms.clipboard,
            fetch.target,
            fetch.property,
            fetch.time,
        )?;
        Ok(())
    }

    /// The owner refused the target asked for: ask for the next, or give
    /// up with no text.
    fn ask_next(&mut self) -> Result<(), Fault> {
        let Some(fetch) = &mut self.fetch else {
            return Ok(());
        };
        if fetch.then.is_empty() {
            debug!("the X clipboard's owner has no text");
            self.finish(None);
            return Ok(());
        }
        fetch.target = fetch.then.remove(0);
        self.ask()
    }

    /// Read and delete `property` of andris's window, if it holds at most
    /// `limit` bytes; `None`, deleting it, if it holds more.
    fn read(&self, property: Atom, limit: usize) -> Result<Option<GetPropertyReply>, Fault> {
        let words = u32::try_from(limit / 4 + 1).unwrap_or(u32::MAX);
        let reply = xproto::get_property(
            &*self.conn,
            true,
            self.window,
            property,
            AtomEnum::ANY,
            0,
            words,
        )?
        .reply()?;
        if reply.bytes_after > 0 || reply.value.len() > limit {
            // A read that leaves bytes behind does not delete.
            xproto::delete_property(&*self.conn, self.window, property)?;
            return Ok(None);
        }
        Ok(Some(reply))
    }

    /// The owner's answer to a conversion andris asked for.
    fn converted(&mut self, event: &SelectionNotifyEvent) -> Result<(), Fault> {
        let Some(fetch) = &self.fetch else {
            return Ok(());
        };
        if !fetch.answered_by(event, self.atoms.clipboard) {
            debug!("ignored a stale answer from a clipboard owner");
            return Ok(());
        }
        if event.property == NONE {
            return self.ask_next();
        }
        let (property, target) = (fetch.property, fetch.target);
        let Some(reply) = self.read(property, MAX_CLIPBOARD_TEXT)? else {
            self.finish_oversized();
            return Ok(());
        };
        if reply.type_ == self.atoms.incr {
            // Reading deleted the property, which asks the owner for the
            // first piece. Its value is a lower bound on the size.
            let bound = atoms_in(reply.format, &reply.value)
                .first()
                .copied()
                .unwrap_or(0);
            if usize::try_from(bound).unwrap_or(usize::MAX) > MAX_CLIPBOARD_TEXT {
                self.finish_oversized();
                return Ok(());
            }
            if let Some(fetch) = &mut self.fetch {
                fetch.incoming = Some(Incoming::default());
                fetch.deadline = Instant::now() + CONVERSION_TIMEOUT;
            }
            return Ok(());
        }
        self.received(target, reply.type_, reply.format, &reply.value)
    }

    /// Give up on text over the limit, saying so.
    fn finish_oversized(&mut self) {
        warn!(
            "not offering the X clipboard's text: it is over the \
             {MAX_CLIPBOARD_TEXT}-byte limit"
        );
        self.finish(None);
    }

    /// The whole of an answer for `target`, of type `type_`.
    fn received(
        &mut self,
        target: Atom,
        type_: Atom,
        format: u8,
        bytes: &[u8],
    ) -> Result<(), Fault> {
        if target == self.atoms.targets {
            let wanted = wanted_targets(&self.atoms, &atoms_in(format, bytes));
            if let Some(fetch) = &mut self.fetch {
                fetch.then = wanted;
            }
            return self.ask_next();
        }
        match decode(&self.atoms, type_, bytes) {
            Some(text) => {
                self.finish(Some(text));
                Ok(())
            }
            None => self.ask_next(),
        }
    }

    /// The next piece of an INCR transfer to andris has arrived.
    fn incoming_piece(&mut self) -> Result<(), Fault> {
        let Some(fetch) = &self.fetch else {
            return Ok(());
        };
        let Some(incoming) = &fetch.incoming else {
            return Ok(());
        };
        let room = MAX_CLIPBOARD_TEXT.saturating_sub(incoming.bytes.len());
        let (property, target) = (fetch.property, fetch.target);
        let Some(reply) = self.read(property, room)? else {
            self.finish_oversized();
            return Ok(());
        };
        let Some(fetch) = &mut self.fetch else {
            return Ok(());
        };
        let Some(incoming) = &mut fetch.incoming else {
            return Ok(());
        };
        if !reply.value.is_empty() {
            incoming.type_ = reply.type_;
            incoming.format = reply.format;
            incoming.bytes.extend_from_slice(&reply.value);
            fetch.deadline = Instant::now() + CONVERSION_TIMEOUT;
            return Ok(());
        }
        // An empty piece ends the transfer.
        let incoming = std::mem::take(incoming);
        fetch.incoming = None;
        self.received(target, incoming.type_, incoming.format, &incoming.bytes)
    }

    /// A property changed: andris's time stamp, a piece of a transfer to
    /// andris, or a requestor taking a piece of one from andris.
    fn property_changed(
        &mut self,
        window: Window,
        atom: Atom,
        state: Property,
        time: Timestamp,
    ) -> Result<(), Fault> {
        if window != self.window {
            if state == Property::DELETE {
                return self.next_piece(window, atom);
            }
            return Ok(());
        }
        if state != Property::NEW_VALUE {
            return Ok(());
        }
        if atom == self.atoms.stamp && self.acquiring.is_some() {
            return self.acquire(time);
        }
        if self
            .fetch
            .as_ref()
            .is_some_and(|fetch| fetch.property == atom && fetch.incoming.is_some())
        {
            return self.incoming_piece();
        }
        Ok(())
    }

    /// Take the selection at server time `time`, for the queued text.
    fn acquire(&mut self, time: Timestamp) -> Result<(), Fault> {
        let Some(text) = self.acquiring.take() else {
            return Ok(());
        };
        xproto::set_selection_owner(&*self.conn, self.window, self.atoms.clipboard, time)?;
        let owner = xproto::get_selection_owner(&*self.conn, self.atoms.clipboard)?
            .reply()?
            .owner;
        if owner != self.window {
            warn!(
                "andris could not take the X clipboard: another program took it at the same time"
            );
            return Ok(());
        }
        self.fetch = None;
        debug!(bytes = text.len(), "andris owns the X clipboard");
        self.owned = Some(Owned::new(&text, time));
        // The text another program put there is gone. Left published, a
        // client connecting later would be offered it as the desktop's.
        self.publish(None);
        Ok(())
    }

    /// Another program took the selection from andris.
    fn cleared(&mut self, event: &SelectionClearEvent) {
        if event.selection == self.atoms.clipboard
            && event.owner == self.window
            && self
                .owned
                .as_ref()
                .is_some_and(|owned| !earlier(event.time, owned.since))
        {
            debug!("andris no longer owns the X clipboard");
            self.owned = None;
        }
    }

    /// Answer a request for andris's text.
    fn serve(&mut self, event: &SelectionRequestEvent) -> Result<(), Fault> {
        // Andris never asks itself, so this is another client naming
        // andris's window, which would have andris write to its own
        // properties and later stop watching its own window. It gets no
        // answer at all: a refusal sent there would look like an owner
        // refusing a conversion andris has under way.
        if event.requestor == self.window {
            debug!("ignored a clipboard request naming andris's own window as the requestor");
            return Ok(());
        }
        // Clients from before ICCCM name no property: use the target.
        let property = if event.property == NONE {
            event.target
        } else {
            event.property
        };
        let answered = self.answer(event, property)?;
        let reply = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: event.time,
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property: if answered { property } else { NONE },
        };
        xproto::send_event(
            &*self.conn,
            false,
            event.requestor,
            EventMask::NO_EVENT,
            reply,
        )?;
        Ok(())
    }

    /// Write the answer to `event` in `property`, returning whether there
    /// is one.
    fn answer(&mut self, event: &SelectionRequestEvent, property: Atom) -> Result<bool, Fault> {
        let Some(owned) = &self.owned else {
            return Ok(false);
        };
        if event.selection != self.atoms.clipboard
            || event.owner != self.window
            || (event.time != CURRENT_TIME && earlier(event.time, owned.since))
        {
            return Ok(false);
        }
        let atoms = self.atoms;
        let requestor = event.requestor;
        if event.target == atoms.targets {
            let targets = offered_targets(&atoms, owned.latin1.is_some());
            self.conn.change_property32(
                PropMode::REPLACE,
                requestor,
                property,
                AtomEnum::ATOM,
                &targets,
            )?;
            return Ok(true);
        }
        if event.target == atoms.timestamp {
            self.conn.change_property32(
                PropMode::REPLACE,
                requestor,
                property,
                AtomEnum::INTEGER,
                &[owned.since],
            )?;
            return Ok(true);
        }
        let data = if event.target == atoms.utf8_string || event.target == atoms.text_plain {
            owned.utf8.clone()
        } else if event.target == string_atom()
            && let Some(latin1) = &owned.latin1
        {
            latin1.clone()
        } else {
            // MULTIPLE among them.
            return Ok(false);
        };
        self.send(requestor, property, event.target, data)
    }

    /// Write `data`, of type `type_`, to `requestor`'s `property`: whole,
    /// or as the start of an INCR transfer.
    fn send(
        &mut self,
        requestor: Window,
        property: Atom,
        type_: Atom,
        data: Arc<[u8]>,
    ) -> Result<bool, Fault> {
        if data.len() <= self.chunk {
            self.conn
                .change_property8(PropMode::REPLACE, requestor, property, type_, &data)?;
            return Ok(true);
        }
        let key = (requestor, property);
        if !self.transfers.contains_key(&key) && self.transfers.len() >= MAX_TRANSFERS {
            warn!(
                "refused a paste from the X clipboard: {MAX_TRANSFERS} large pastes are \
                 already under way"
            );
            return Ok(false);
        }
        // Before the INCR property, so the requestor's deletion of it is
        // seen.
        xproto::change_window_attributes(
            &*self.conn,
            requestor,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        )?;
        let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
        self.conn.change_property32(
            PropMode::REPLACE,
            requestor,
            property,
            self.atoms.incr,
            &[size],
        )?;
        self.transfers.insert(
            key,
            Transfer {
                type_,
                data,
                sent: 0,
                deadline: Instant::now() + TRANSFER_TIMEOUT,
            },
        );
        Ok(true)
    }

    /// A requestor deleted `property` of `window`: if that is an INCR
    /// transfer from andris, write the next piece, or the empty piece that
    /// ends it.
    fn next_piece(&mut self, window: Window, property: Atom) -> Result<(), Fault> {
        let Some(transfer) = self.transfers.get_mut(&(window, property)) else {
            return Ok(());
        };
        let range = transfer.next_piece(self.chunk);
        let done = range.is_empty();
        self.conn.change_property8(
            PropMode::REPLACE,
            window,
            property,
            transfer.type_,
            &transfer.data[range],
        )?;
        if done {
            self.transfers.remove(&(window, property));
            self.unwatch_if_idle(window)?;
        } else {
            transfer.deadline = Instant::now() + TRANSFER_TIMEOUT;
        }
        Ok(())
    }

    /// Stop following `window`'s events once no transfer to it remains.
    fn unwatch_if_idle(&self, window: Window) -> Result<(), Fault> {
        // Andris's own window keeps its events whatever happens.
        if window != self.window && !self.transfers.keys().any(|(other, _)| *other == window) {
            xproto::change_window_attributes(
                &*self.conn,
                window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atoms() -> Atoms {
        Atoms {
            clipboard: 300,
            targets: 301,
            timestamp: 302,
            utf8_string: 303,
            text_plain: 304,
            incr: 305,
            stamp: 306,
            slots: [307, 308, 309, 310],
        }
    }

    #[test]
    fn latin1_round_trips_and_refuses_what_it_lacks() {
        assert_eq!(to_latin1("café ÿ").unwrap(), b"caf\xe9 \xff");
        assert_eq!(from_latin1(b"caf\xe9 \xff"), "café ÿ");
        assert_eq!(from_latin1(&to_latin1("plain\n").unwrap()), "plain\n");
        assert_eq!(to_latin1("Ā"), None, "U+0100 is past Latin-1");
        assert_eq!(to_latin1("世界"), None);
        assert_eq!(to_latin1("").unwrap(), b"");
        // Every byte is a character, two UTF-8 bytes above 0x7f.
        let all: Vec<u8> = (0..=255).collect();
        let text = from_latin1(&all);
        assert_eq!(text.chars().count(), 256);
        assert_eq!(text.len(), 128 + 2 * 128);
        assert_eq!(to_latin1(&text).unwrap(), all);
    }

    #[test]
    fn targets_offer_string_only_for_latin1_text() {
        let atoms = atoms();
        let latin1 = offered_targets(&atoms, true);
        assert_eq!(latin1, vec![301, 302, 303, 304, string_atom()]);
        let wide = offered_targets(&atoms, false);
        assert_eq!(wide, vec![301, 302, 303, 304]);
        assert!(!wide.contains(&string_atom()));
    }

    #[test]
    fn utf8_is_wanted_before_string_and_nothing_else_is() {
        let atoms = atoms();
        let string = string_atom();
        assert_eq!(
            wanted_targets(&atoms, &[301, string, 303]),
            vec![303, string]
        );
        assert_eq!(wanted_targets(&atoms, &[301, string]), vec![string]);
        assert_eq!(wanted_targets(&atoms, &[301, 302, 999]), Vec::<Atom>::new());
        assert_eq!(wanted_targets(&atoms, &[]), Vec::<Atom>::new());
    }

    #[test]
    fn properties_decode_by_type() {
        let atoms = atoms();
        assert_eq!(
            decode(&atoms, string_atom(), b"caf\xe9").as_deref(),
            Some("café")
        );
        assert_eq!(
            decode(&atoms, 303, "café".as_bytes()).as_deref(),
            Some("café")
        );
        assert_eq!(decode(&atoms, 304, b"plain").as_deref(), Some("plain"));
        assert_eq!(
            decode(&atoms, 303, b"bad \xff").as_deref(),
            Some("bad \u{fffd}")
        );
        assert_eq!(decode(&atoms, 999, b"image"), None);
    }

    #[test]
    fn atoms_are_read_only_from_32_bit_properties() {
        let bytes: Vec<u8> = [301u32, 303].iter().flat_map(|a| a.to_ne_bytes()).collect();
        assert_eq!(atoms_in(32, &bytes), vec![301, 303]);
        assert_eq!(atoms_in(8, &bytes), Vec::<Atom>::new());
        // A trailing partial word is ignored.
        assert_eq!(atoms_in(32, &bytes[..7]), vec![301]);
    }

    #[test]
    fn chunks_fit_in_a_request_and_are_capped() {
        // BIG-REQUESTS: 16 MiB requests, capped.
        assert_eq!(chunk_size(4_194_303 * 4), INCR_CHUNK);
        // The core protocol's 256 KiB.
        assert_eq!(chunk_size(65_535 * 4), 65_535 * 4 - 28);
        assert_eq!(chunk_size(0), 1);
    }

    #[test]
    fn incr_pieces_cover_the_data_then_end_empty() {
        let mut transfer = Transfer {
            type_: 303,
            data: Arc::from(vec![7u8; 25].as_slice()),
            sent: 0,
            deadline: Instant::now(),
        };
        let pieces: Vec<Range<usize>> = (0..5).map(|_| transfer.next_piece(10)).collect();
        assert_eq!(pieces, vec![0..10, 10..20, 20..25, 25..25, 25..25]);

        let mut exact = Transfer {
            type_: 303,
            data: Arc::from(vec![7u8; 20].as_slice()),
            sent: 0,
            deadline: Instant::now(),
        };
        let pieces: Vec<Range<usize>> = (0..3).map(|_| exact.next_piece(10)).collect();
        assert_eq!(pieces, vec![0..10, 10..20, 20..20]);
    }

    #[test]
    fn timestamps_compare_across_the_wrap() {
        assert!(earlier(1, 2));
        assert!(!earlier(2, 1));
        assert!(!earlier(5, 5));
        assert!(earlier(u32::MAX, 3), "just before the wrap is earlier");
        assert!(!earlier(3, u32::MAX));
        assert!(earlier(0, 1 << 30));
    }

    #[test]
    fn deadlines_pick_the_earliest_and_find_the_stalled() {
        let now = Instant::now();
        let transfer = |at| Transfer {
            type_: 303,
            data: Arc::from(&b""[..]),
            sent: 0,
            deadline: at,
        };
        let mut transfers = HashMap::new();
        assert_eq!(earliest(None, &transfers), None);
        let soon = now + Duration::from_secs(1);
        let later = now + Duration::from_secs(5);
        assert_eq!(earliest(Some(later), &transfers), Some(later));
        transfers.insert((1, 2), transfer(soon));
        transfers.insert((3, 4), transfer(later));
        assert_eq!(earliest(None, &transfers), Some(soon));
        assert_eq!(earliest(Some(now), &transfers), Some(now));

        assert!(stalled(&transfers, now).is_empty());
        assert_eq!(stalled(&transfers, soon), vec![(1, 2)]);
        let mut both = stalled(&transfers, later);
        both.sort();
        assert_eq!(both, vec![(1, 2), (3, 4)]);
    }

    #[test]
    fn only_the_answer_to_the_current_conversion_counts() {
        let fetch = Fetch {
            property: 307,
            time: 1000,
            target: 303,
            then: vec![],
            incoming: None,
            deadline: Instant::now(),
        };
        let answer = |time, target, property| SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time,
            requestor: 1,
            selection: 300,
            target,
            property,
        };
        assert!(fetch.answered_by(&answer(1000, 303, 307), 300));
        assert!(
            fetch.answered_by(&answer(1000, 303, NONE), 300),
            "a refusal"
        );
        assert!(
            fetch.answered_by(&answer(CURRENT_TIME, 303, 307), 300),
            "an owner that answers with CurrentTime"
        );
        assert!(
            !fetch.answered_by(&answer(999, 303, 307), 300),
            "an earlier ask"
        );
        assert!(
            !fetch.answered_by(&answer(1000, 301, 307), 300),
            "another target"
        );
        assert!(
            !fetch.answered_by(&answer(1000, 303, 308), 300),
            "another slot"
        );
        assert!(!fetch.answered_by(&answer(1000, 303, 307), 1), "PRIMARY");

        let any_time = Fetch {
            time: CURRENT_TIME,
            ..fetch
        };
        assert!(any_time.answered_by(&answer(1234, 303, 307), 300));
        let receiving = Fetch {
            incoming: Some(Incoming::default()),
            ..any_time
        };
        assert!(
            !receiving.answered_by(&answer(1234, 303, 307), 300),
            "an INCR transfer is already answering"
        );
    }
}
