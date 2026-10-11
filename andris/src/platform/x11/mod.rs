//! The X11 backend: captures an existing X display, injects input into
//! it, and follows its cursor, through x11rb.
//!
//! # Threads
//!
//! x11rb's `RustConnection` is synchronous, so the backend opens three
//! connections to the display, each owned by its own thread:
//!
//! - **capture** answers size and capture requests with `GetGeometry` and
//!   `GetImage` on the root window;
//! - **input** injects events with XTest and reads the lock state from
//!   XKB's indicators. Both go through one queue, so a lock-state read
//!   sees every event queued before it;
//! - **cursor** blocks on XFixes cursor events, and publishes each new
//!   shape through a `watch` channel.
//!
//! Separate connections keep a key press from queueing behind a
//! multi-megabyte `GetImage`. The threads talk to the rest of andris
//! through `std::sync::mpsc` queues, so [`InputSink::inject`] only
//! enqueues and never blocks.
//!
//! A thread whose connection fails logs that once and exits; every later
//! call to it returns an error. An error the X server returns for one
//! request fails that request only.
//!
//! Dropping the platform closes the capture and input queues, and those
//! threads exit. The cursor thread is blocked waiting for an X event, so
//! the drop also sends a `ClientMessage` to an unmapped window only that
//! thread knows, which wakes it to exit.
//!
//! # Keys
//!
//! Keys arrive as evdev codes, and become this server's keycodes by XKB
//! key name; see [`keymap`]. The server's names are read once, at
//! startup: a keymap changed later (with `setxkbmap`, say) is not seen.

mod checks;
mod evdev_names;
mod keymap;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;

use anyhow::{Context, Result, anyhow};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageEvent, CreateWindowAux, EventMask, ImageFormat, ImageOrder,
    VisualClass, Window, WindowClass,
};
use x11rb::protocol::{xfixes, xkb, xtest};
use x11rb::rust_connection::RustConnection;

use super::{
    Button, CursorReceiver, CursorShape, CursorSource, DisplayGeometry, Frame, FrameSource,
    InputEvent, InputSink, LockState, Platform, Size,
};
use checks::ServerFacts;
use keymap::{KeyName, Keymap, Unmapped};

/// The X11 platform. Create it with [`X11Platform::connect`] and hand the
/// server [`X11Platform::platform`].
pub struct X11Platform {
    capture: Sender<CaptureRequest>,
    input: Sender<InputRequest>,
    cursor: CursorReceiver,
    /// The cursor thread's connection, kept to wake that thread on drop.
    cursor_conn: Arc<RustConnection>,
    /// The window the wake-up is sent to.
    wake_window: Window,
}

impl X11Platform {
    /// Connect to X display `name` (`:10`, say), check that andris can
    /// export it, and start the backend's threads.
    ///
    /// Fails, saying why, if the display cannot be reached, if its root
    /// window is not 32-bit BGRX, or if it lacks XTEST, XFIXES or
    /// XKEYBOARD.
    pub fn connect(name: &str) -> Result<Arc<Self>> {
        // The input connection first: the checks need its XKB handshake.
        let (input_conn, screen) = open(name, "input")?;
        let screen = input_conn
            .setup()
            .roots
            .get(screen)
            .cloned()
            .with_context(|| format!("X display {name} has no screen {screen}"))?;
        let facts = server_facts(&input_conn, &screen)
            .with_context(|| format!("asking X display {name} what it supports"))?;
        checks::check(name, &facts)?;
        let root = screen.root;
        info!(
            depth = screen.root_depth,
            "connected to X display {name}; it can be exported"
        );
        let injector = Injector::new(input_conn, root)?;

        let (capture_conn, _) = open(name, "capture")?;

        let (cursor_conn, _) = open(name, "the cursor")?;
        let cursor_conn = Arc::new(cursor_conn);
        let (wake_window, first_shape) = start_cursor(&cursor_conn, root)
            .with_context(|| format!("following the cursor on X display {name}"))?;
        let (shapes, cursor) = watch::channel(first_shape.map(Arc::new));

        let (capture, capture_queue) = mpsc::channel();
        spawn("andris-x11-capture", move || {
            serve_capture(capture_conn, root, capture_queue)
        })?;
        let (input, input_queue) = mpsc::channel();
        spawn("andris-x11-input", move || {
            serve_input(injector, input_queue)
        })?;
        let thread_conn = cursor_conn.clone();
        spawn("andris-x11-cursor", move || {
            serve_cursor(thread_conn, wake_window, shapes)
        })?;

        Ok(Arc::new(Self {
            capture,
            input,
            cursor,
            cursor_conn,
            wake_window,
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
}

impl Drop for X11Platform {
    fn drop(&mut self) {
        // The capture and input threads stop when their queues close,
        // which happens as the fields drop. Wake the cursor thread, which
        // is waiting for an X event. If its connection has gone it has
        // already stopped, so a failure here does not matter.
        let event = ClientMessageEvent::new(32, self.wake_window, AtomEnum::NONE, [0u32; 5]);
        let sent = xproto::send_event(
            &*self.cursor_conn,
            false,
            self.wake_window,
            EventMask::NO_EVENT,
            event,
        );
        if sent.is_ok() {
            let _ = self.cursor_conn.flush();
        }
    }
}

impl DisplayGeometry for X11Platform {
    fn size(&self) -> Result<Size> {
        round_trip(&self.capture, "capture", CaptureRequest::Size)
    }
}

impl FrameSource for X11Platform {
    fn capture(&self) -> Result<Frame> {
        round_trip(&self.capture, "capture", CaptureRequest::Capture)
    }
}

impl InputSink for X11Platform {
    fn inject(&self, event: InputEvent) -> Result<()> {
        self.input
            .send(InputRequest::Event(event))
            .map_err(|_| stopped("input"))
    }

    fn lock_state(&self) -> Result<LockState> {
        round_trip(&self.input, "input", InputRequest::LockState)
    }
}

impl CursorSource for X11Platform {
    fn subscribe(&self) -> CursorReceiver {
        let mut receiver = self.cursor.clone();
        receiver.mark_unchanged();
        receiver
    }
}

/// A request to the capture thread, with where to send its answer.
enum CaptureRequest {
    Size(SyncSender<Result<Size>>),
    Capture(SyncSender<Result<Frame>>),
}

/// A request to the input thread.
enum InputRequest {
    /// Inject an event; there is no answer.
    Event(InputEvent),
    /// Read the lock state, after every event queued before this.
    LockState(SyncSender<Result<LockState>>),
}

/// The error for a call to a backend thread that has stopped.
fn stopped(thread: &str) -> anyhow::Error {
    anyhow!("the X11 {thread} thread has stopped, so the X display is no longer available")
}

/// Send `queue` the request `make` builds around a reply channel, and
/// wait for the answer.
fn round_trip<R, T>(
    queue: &Sender<R>,
    thread: &str,
    make: impl FnOnce(SyncSender<Result<T>>) -> R,
) -> Result<T> {
    let (reply, answer) = mpsc::sync_channel(1);
    queue.send(make(reply)).map_err(|_| stopped(thread))?;
    answer.recv().map_err(|_| stopped(thread))?
}

/// Start a named backend thread, which runs until `body` returns.
fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Result<()> {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
        .with_context(|| format!("starting thread {name}"))?;
    Ok(())
}

/// Open a connection to `display`, for `role`.
fn open(display: &str, role: &str) -> Result<(RustConnection, usize)> {
    x11rb::connect(Some(display))
        .with_context(|| format!("connecting to X display {display} for {role}"))
}

/// Why a request to the X server failed.
#[derive(Debug)]
enum Fault {
    /// The connection has failed; the thread that owns it must stop.
    Gone(anyhow::Error),
    /// The X server refused this request; the connection still works.
    Failed(anyhow::Error),
}

impl From<ConnectionError> for Fault {
    fn from(e: ConnectionError) -> Self {
        Fault::Gone(anyhow::Error::new(e).context("the connection to the X display failed"))
    }
}

impl From<ReplyError> for Fault {
    fn from(e: ReplyError) -> Self {
        match e {
            ReplyError::ConnectionError(e) => e.into(),
            ReplyError::X11Error(e) => Fault::Failed(anyhow!(
                "the X server returned {:?} for request {}.{}",
                e.error_kind,
                e.major_opcode,
                e.minor_opcode
            )),
        }
    }
}

/// Send a request's result to whoever asked. Returns the error if the
/// connection has failed, after which the calling thread must stop.
fn answer<T>(reply: SyncSender<Result<T>>, result: Result<T, Fault>) -> Option<anyhow::Error> {
    // A send fails only if the caller has stopped waiting, which is not
    // this thread's problem.
    match result {
        Ok(value) => {
            let _ = reply.send(Ok(value));
            None
        }
        Err(Fault::Failed(e)) => {
            let _ = reply.send(Err(e));
            None
        }
        Err(Fault::Gone(e)) => {
            let _ = reply.send(Err(anyhow!("{e:#}")));
            Some(e)
        }
    }
}

/// What the display's setup and extensions say, for [`checks::check`].
/// Makes the XKB `UseExtension` handshake on `conn`, which is the input
/// connection's first XKB request.
fn server_facts(conn: &RustConnection, screen: &xproto::Screen) -> Result<ServerFacts> {
    let setup = conn.setup();
    let bits_per_pixel = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == screen.root_depth)
        .map(|format| format.bits_per_pixel);
    let rgb_masks = screen
        .allowed_depths
        .iter()
        .flat_map(|depth| &depth.visuals)
        .find(|visual| visual.visual_id == screen.root_visual)
        .filter(|visual| {
            visual.class == VisualClass::TRUE_COLOR || visual.class == VisualClass::DIRECT_COLOR
        })
        .map(|visual| (visual.red_mask, visual.green_mask, visual.blue_mask));
    let present = |name| -> Result<bool> { Ok(conn.extension_information(name)?.is_some()) };
    let xkb = present(xkb::X11_EXTENSION_NAME)?
        && xkb::use_extension(conn, 1, 0)?
            .reply()
            .context("the XKB UseExtension handshake")?
            .supported;
    Ok(ServerFacts {
        root_depth: screen.root_depth,
        bits_per_pixel,
        lsb_first: setup.image_byte_order == ImageOrder::LSB_FIRST,
        rgb_masks,
        xtest: present(xtest::X11_EXTENSION_NAME)?,
        xfixes: present(xfixes::X11_EXTENSION_NAME)?,
        xkb,
    })
}

// --- Capture -----------------------------------------------------------

/// Serve capture requests until the queue closes or the connection fails.
fn serve_capture(conn: RustConnection, root: Window, requests: Receiver<CaptureRequest>) {
    while let Ok(request) = requests.recv() {
        let gone = match request {
            CaptureRequest::Size(reply) => answer(reply, root_size(&conn, root).map(|s| s.0)),
            CaptureRequest::Capture(reply) => answer(reply, capture(&conn, root)),
        };
        if let Some(e) = gone {
            error!(
                error = format!("{e:#}"),
                "the X11 capture thread lost its connection; capture has stopped"
            );
            return;
        }
    }
    debug!("the X11 capture thread is stopping");
}

/// The root window's size now, as a checked [`Size`] and as X's `u16`s.
fn root_size(conn: &RustConnection, root: Window) -> Result<(Size, u16, u16), Fault> {
    let geometry = xproto::get_geometry(conn, root)?.reply()?;
    let size = Size {
        width: geometry.width.into(),
        height: geometry.height.into(),
    };
    size.check().map_err(Fault::Failed)?;
    Ok((size, geometry.width, geometry.height))
}

/// Capture the root window at its current size.
///
/// The checks at startup ensured a `ZPixmap` image here is 32 bits a
/// pixel, LSBFirst, with red, green and blue masks that put the bytes
/// in memory as B, G, R and one more. At depth 24 X does not promise
/// that fourth byte is zero, but [`Frame`] ignores it. A 32 bit pixel
/// fills a scanline unit whatever the scanline pad, so rows are
/// `width * 4` bytes apart.
fn capture(conn: &RustConnection, root: Window) -> Result<Frame, Fault> {
    let (_, width, height) = root_size(conn, root)?;
    let image =
        xproto::get_image(conn, ImageFormat::Z_PIXMAP, root, 0, 0, width, height, !0)?.reply()?;
    Frame::new(
        width.into(),
        height.into(),
        usize::from(width) * 4,
        image.data,
    )
    .map_err(|e| Fault::Failed(e.context("the X server's image is not the size it said")))
}

// --- Input -------------------------------------------------------------

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
struct Injector {
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
    fn new(conn: RustConnection, root: Window) -> Result<Self> {
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
/// thread wakes.
fn serve_input(mut injector: Injector, requests: Receiver<InputRequest>) {
    while let Ok(first) = requests.recv() {
        let mut next = Some(first);
        while let Some(request) = next {
            let gone = match request {
                InputRequest::Event(event) => injector.inject(event).err().map(|e| {
                    anyhow::Error::new(e).context("the connection to the X display failed")
                }),
                InputRequest::LockState(reply) => answer(reply, injector.lock_state()),
            };
            if let Some(e) = gone {
                error!(
                    error = format!("{e:#}"),
                    "the X11 input thread lost its connection; input has stopped"
                );
                return;
            }
            next = requests.try_recv().ok();
        }
        if let Err(e) = injector.finish_batch() {
            error!(
                error = format!("{e:#}"),
                "the X11 input thread lost its connection; input has stopped"
            );
            return;
        }
    }
    debug!("the X11 input thread is stopping");
}

// --- Cursor ------------------------------------------------------------

/// Set up the cursor connection: make the XFixes handshake, create the
/// window the drop wakes the thread through, ask for cursor changes, and
/// read the current shape (`None` if it cannot be used).
fn start_cursor(conn: &RustConnection, root: Window) -> Result<(Window, Option<CursorShape>)> {
    xfixes::query_version(conn, 5, 0)?
        .reply()
        .context("the XFIXES QueryVersion handshake")?;
    let wake_window = conn.generate_id()?;
    xproto::create_window(
        conn,
        x11rb::COPY_FROM_PARENT as u8,
        wake_window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new(),
    )?;
    xfixes::select_cursor_input(conn, root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    let shape = match read_cursor(conn) {
        Ok(shape) => Some(shape),
        Err(Fault::Gone(e)) => return Err(e),
        Err(Fault::Failed(e)) => {
            warn!(
                error = format!("{e:#}"),
                "the X cursor cannot be read; starting with none"
            );
            None
        }
    };
    Ok((wake_window, shape))
}

/// The current cursor image.
fn read_cursor(conn: &RustConnection) -> Result<CursorShape, Fault> {
    let image = xfixes::get_cursor_image(conn)?.reply()?;
    // The reply also carries the pointer's position, `image.x` and
    // `image.y`, for when andris sends the cursor's position as well as
    // its shape.
    CursorShape::new(
        image.width,
        image.height,
        image.xhot,
        image.yhot,
        image.cursor_image,
    )
    .map_err(|e| Fault::Failed(e.context("the X server's cursor image is not usable")))
}

/// Follow cursor changes until woken to stop or the connection fails.
/// The thread owns the only sender, so its receivers see it stop.
fn serve_cursor(
    conn: Arc<RustConnection>,
    wake_window: Window,
    shapes: watch::Sender<Option<Arc<CursorShape>>>,
) {
    let mut warned = false;
    loop {
        let event = match conn.wait_for_event() {
            Ok(event) => event,
            Err(e) => {
                error!(
                    error = format!("{e:#}"),
                    "the X11 cursor thread lost its connection; cursor changes have stopped"
                );
                return;
            }
        };
        let failure = match event {
            Event::XfixesCursorNotify(_) => match read_cursor(&conn) {
                Ok(shape) => {
                    shapes.send_replace(Some(Arc::new(shape)));
                    None
                }
                Err(Fault::Gone(e)) => {
                    error!(
                        error = format!("{e:#}"),
                        "the X11 cursor thread lost its connection; cursor changes have stopped"
                    );
                    return;
                }
                Err(Fault::Failed(e)) => Some(e),
            },
            Event::ClientMessage(message) if message.window == wake_window => {
                debug!("the X11 cursor thread is stopping");
                return;
            }
            Event::Error(e) => Some(anyhow!(
                "the X server returned {:?} for request {}.{}",
                e.error_kind,
                e.major_opcode,
                e.minor_opcode
            )),
            _ => None,
        };
        if let Some(e) = failure
            && !warned
        {
            warned = true;
            warn!(
                error = format!("{e:#}"),
                "reading the X cursor failed; later failures are not logged"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_dead_thread_makes_every_call_fail() {
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        drop(requests);
        let message = round_trip(&queue, "capture", CaptureRequest::Size)
            .unwrap_err()
            .to_string();
        assert!(message.contains("capture thread has stopped"), "{message}");

        // A thread that takes the request but dies before answering.
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        let worker = thread::spawn(move || drop(requests.recv()));
        assert!(round_trip(&queue, "capture", CaptureRequest::Capture).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn a_failed_connection_stops_the_thread_but_answers_first() {
        let (reply, answered) = mpsc::sync_channel(1);
        let gone = answer::<()>(reply, Err(Fault::from(ConnectionError::UnknownError)));
        assert!(gone.is_some());
        assert!(answered.recv().unwrap().is_err());

        let (reply, answered) = mpsc::sync_channel(1);
        let gone = answer::<()>(reply, Err(Fault::Failed(anyhow!("BadMatch"))));
        assert!(
            gone.is_none(),
            "a refused request leaves the thread running"
        );
        assert!(answered.recv().unwrap().is_err());
    }
}
