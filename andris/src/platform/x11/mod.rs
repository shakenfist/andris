//! The X11 backend: captures an existing X display, injects input into
//! it, and follows its cursor, through x11rb.
//!
//! # Threads
//!
//! x11rb's `RustConnection` is synchronous, so the backend opens four
//! connections to the display, each owned by its own thread:
//!
//! - **capture** answers size and capture requests with `GetGeometry` on
//!   the root window, and one image per rectangle asked for: with
//!   MIT-SHM's `ShmGetImage` into a memfd shared with the server where
//!   that works, and with `GetImage` otherwise (see [`capture`] and
//!   [`shm`]). Which is used, and why, is logged once at startup;
//! - **input** injects events with XTest and reads the lock state from
//!   XKB's indicators. Both go through one queue, so a lock-state read
//!   sees every event queued before it;
//! - **cursor** blocks on XFixes cursor events, and publishes each new
//!   shape through a `watch` channel;
//! - **damage** blocks on XDamage events from a `RawRectangles` damage
//!   object on the root window, and on the root window's
//!   `ConfigureNotify`, and reports them to the platform's
//!   [`DamageHub`]: damage as damage, a new size as a resize (see
//!   [`damage`]).
//!
//! Separate connections keep a key press from queueing behind a
//! multi-megabyte capture, and let the cursor and damage threads block
//! on events while the capture thread serves round trips. The threads talk to the rest of andris
//! through `std::sync::mpsc` queues, so [`InputSink::inject`] only
//! enqueues and never blocks.
//!
//! The input queue is bounded, so a client flooding keys cannot grow it
//! without limit: [`InputSink::inject`] fails once [`INPUT_QUEUE_LIMIT`]
//! events wait, and the inputs channel that called it closes. Releases
//! andris owes the desktop go through [`InputSink::inject_release`], which
//! may use [`RELEASE_RESERVE`] more. Both share one FIFO, so a release
//! never overtakes the press it undoes; see [`InputQueue`].
//!
//! [`InputQueue`]: input::InputQueue
//!
//! A thread whose connection fails logs that once and exits; every later
//! call to it returns an error. When the damage thread's fails, the hub
//! is told the desktop has [`Gone`], so every [`DamageSubscription`]
//! learns of it. An error the X server returns for one request fails that
//! request only.
//!
//! Dropping the platform closes the capture and input queues, and those
//! threads exit; the capture thread detaches its MIT-SHM segment as it
//! goes. The cursor and damage threads are blocked waiting for X events,
//! so the drop also sends each a `ClientMessage` to an unmapped window
//! only that thread knows, which wakes it to exit.
//!
//! # Keys
//!
//! Keys arrive as evdev codes, and become this server's keycodes by XKB
//! key name; see [`keymap`]. The server's names are read once, at
//! startup: a keymap changed later (with `setxkbmap`, say) is not seen.

mod capture;
mod checks;
mod cursor;
mod damage;
mod evdev_names;
mod input;
mod keymap;
// The crate's only unsafe code: the shared memory mapping. See the
// module's documentation for why it is sound.
#[allow(unsafe_code)]
mod shm;

use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;

use anyhow::{Context, Result, anyhow};
use tokio::sync::watch;
use tracing::{error, info};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, CreateWindowAux, EventMask, ImageOrder, VisualClass,
    Window, WindowClass,
};
use x11rb::protocol::{damage as xdamage, shm as xshm, xfixes, xkb, xtest};
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::X11Error;

use super::{
    Capture, CursorReceiver, CursorSource, DamageHub, DamageSubscription, DisplayGeometry, Gone,
    InputEvent, InputSink, LockState, Platform, Rectangle, ScreenSource, Size,
};
use capture::{CaptureRequest, Capturer, serve_capture};
use checks::ServerFacts;
use cursor::{serve_cursor, start_cursor};
use damage::{DamageThread, serve_damage, start_damage};
use input::{Injector, InputQueue, InputRequest, serve_input};

/// How many events [`InputSink::inject`] lets wait for the input thread.
pub const INPUT_QUEUE_LIMIT: usize = 8192;

/// How many more events [`InputSink::inject_release`] may queue beyond
/// [`INPUT_QUEUE_LIMIT`]. Ample for every key and button held at once.
pub const RELEASE_RESERVE: usize = 512;

/// How [`X11Platform::connect_with`] sets the backend up. The default is
/// what [`X11Platform::connect`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X11Options {
    /// Whether capture may use MIT-SHM. With `false` it always uses
    /// `GetImage`, which tests use to compare the two.
    pub shm: bool,
}

impl Default for X11Options {
    fn default() -> Self {
        Self { shm: true }
    }
}

/// How the X11 platform reads the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMethod {
    /// `ShmGetImage` into a memfd shared with the X server.
    Shm,
    /// `GetImage`, whose pixels cross the X socket.
    GetImage,
}

/// A thread blocked on X events, and how to wake it: a `ClientMessage`
/// to a window only it knows, sent on its own connection.
struct Waker {
    conn: Arc<RustConnection>,
    window: Window,
}

impl Waker {
    /// Wake the thread. If its connection has gone the thread has already
    /// stopped, so a failure does not matter.
    fn wake(&self) {
        let event = ClientMessageEvent::new(32, self.window, AtomEnum::NONE, [0u32; 5]);
        let sent = xproto::send_event(&*self.conn, false, self.window, EventMask::NO_EVENT, event);
        if sent.is_ok() {
            let _ = self.conn.flush();
        }
    }
}

/// The X11 platform. Create it with [`X11Platform::connect`] and hand the
/// server [`X11Platform::platform`].
pub struct X11Platform {
    capture: Sender<CaptureRequest>,
    input: InputQueue,
    cursor: CursorReceiver,
    /// Wakes the cursor thread on drop.
    cursor_waker: Waker,
    /// Wakes the damage thread on drop.
    damage_waker: Waker,
    /// The desktop's damage, for every subscriber; the damage thread
    /// reports to it.
    damage: Arc<DamageHub>,
    /// Whether the capture thread is using MIT-SHM.
    using_shm: Arc<AtomicBool>,
}

impl X11Platform {
    /// Connect to X display `name` (`:10`, say), check that andris can
    /// export it, and start the backend's threads, with the default
    /// [`X11Options`].
    ///
    /// Fails, saying why, if the display cannot be reached, if its root
    /// window is not 32-bit BGRX, or if it lacks XTEST, XFIXES, XKEYBOARD
    /// or DAMAGE.
    pub fn connect(name: &str) -> Result<Arc<Self>> {
        Self::connect_with(name, X11Options::default())
    }

    /// [`X11Platform::connect`], set up as `options` says.
    pub fn connect_with(name: &str, options: X11Options) -> Result<Arc<Self>> {
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

        // Damage before capture, so the size the capture segment is made
        // for is one whose changes are already followed.
        let (damage_conn, _) = open(name, "damage")?;
        let damage_conn = Arc::new(damage_conn);
        let (damage_window, damage_object, size) = start_damage(&damage_conn, root)
            .with_context(|| format!("following damage on X display {name}"))?;
        let damage = Arc::new(DamageHub::new(size));

        let (capture_conn, _) = open(name, "capture")?;
        let unix = shm::is_unix_socket(capture_conn.stream().as_fd());
        let using_shm = Arc::new(AtomicBool::new(false));
        let capturer = Capturer::new(
            capture_conn,
            root,
            size,
            shm::refusal(options.shm, facts.shm_fd, unix),
            using_shm.clone(),
        )
        .with_context(|| format!("setting up capture of X display {name}"))?;

        let (cursor_conn, _) = open(name, "the cursor")?;
        let cursor_conn = Arc::new(cursor_conn);
        let (cursor_window, first_shape) = start_cursor(&cursor_conn, root)
            .with_context(|| format!("following the cursor on X display {name}"))?;
        let (shapes, cursor) = watch::channel(first_shape.map(Arc::new));

        let (capture, capture_queue) = mpsc::channel();
        spawn("andris-x11-capture", move || {
            serve_capture(capturer, capture_queue)
        })?;
        let (input, input_queue) = InputQueue::new();
        spawn("andris-x11-input", move || {
            serve_input(injector, input_queue)
        })?;
        let thread_conn = cursor_conn.clone();
        spawn("andris-x11-cursor", move || {
            serve_cursor(thread_conn, cursor_window, shapes)
        })?;
        let thread = DamageThread {
            conn: damage_conn.clone(),
            root,
            wake_window: damage_window,
            damage: damage_object,
            size,
            hub: damage.clone(),
        };
        spawn("andris-x11-damage", move || serve_damage(thread))?;

        Ok(Arc::new(Self {
            capture,
            input,
            cursor,
            cursor_waker: Waker {
                conn: cursor_conn,
                window: cursor_window,
            },
            damage_waker: Waker {
                conn: damage_conn,
                window: damage_window,
            },
            damage,
            using_shm,
        }))
    }

    /// How the screen is being read. MIT-SHM can give way to `GetImage`
    /// after startup, if the desktop grows and the server refuses a
    /// larger segment.
    pub fn capture_method(&self) -> CaptureMethod {
        if self.using_shm.load(Ordering::Relaxed) {
            CaptureMethod::Shm
        } else {
            CaptureMethod::GetImage
        }
    }

    /// This platform as all four seams.
    pub fn platform(self: &Arc<Self>) -> Platform {
        Platform {
            geometry: self.clone(),
            screen: self.clone(),
            input: self.clone(),
            cursor: self.clone(),
        }
    }
}

impl Drop for X11Platform {
    fn drop(&mut self) {
        // The capture and input threads stop when their queues close,
        // which happens as the fields drop. Wake the cursor and damage
        // threads, which are waiting for X events.
        self.cursor_waker.wake();
        self.damage_waker.wake();
    }
}

impl DisplayGeometry for X11Platform {
    fn size(&self) -> Result<Size> {
        round_trip(&self.capture, "capture", CaptureRequest::Size)
    }
}

impl ScreenSource for X11Platform {
    fn subscribe_damage(&self) -> Result<DamageSubscription> {
        self.damage.subscribe()
    }

    fn capture(&self, rects: &[Rectangle]) -> Result<Capture> {
        round_trip(&self.capture, "capture", |reply| {
            CaptureRequest::Capture(Some(rects.to_vec()), reply)
        })
    }

    fn capture_all(&self) -> Result<Capture> {
        round_trip(&self.capture, "capture", |reply| {
            CaptureRequest::Capture(None, reply)
        })
    }
}

impl InputSink for X11Platform {
    fn inject(&self, event: InputEvent) -> Result<()> {
        self.input.push(event, INPUT_QUEUE_LIMIT)
    }

    fn inject_release(&self, event: InputEvent) -> Result<()> {
        self.input.push(event, INPUT_QUEUE_LIMIT + RELEASE_RESERVE)
    }

    fn lock_state(&self) -> Result<LockState> {
        round_trip(&self.input.requests, "input", InputRequest::LockState)
    }
}

impl CursorSource for X11Platform {
    fn subscribe(&self) -> CursorReceiver {
        let mut receiver = self.cursor.clone();
        receiver.mark_unchanged();
        receiver
    }
}

/// The error for a call to a backend thread that has stopped.
fn stopped(thread: &str) -> anyhow::Error {
    anyhow::Error::new(Gone(format!(
        "the X11 {thread} thread has stopped, so the X display is no longer available"
    )))
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

/// Create an unmapped 1x1 input-only window on `conn`, a child of `root`,
/// for a thread blocked on that connection's events to be woken through.
fn create_wake_window(conn: &RustConnection, root: Window) -> Result<Window> {
    let window = conn.generate_id()?;
    xproto::create_window(
        conn,
        x11rb::COPY_FROM_PARENT as u8,
        window,
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
    Ok(window)
}

/// Open a connection to `display`, for `role`.
fn open(display: &str, role: &str) -> Result<(RustConnection, usize)> {
    x11rb::connect(Some(display))
        .with_context(|| format!("connecting to X display {display} for {role}"))
}

/// Log that backend thread `thread` lost its connection to the X display
/// and is stopping, and what stops with it.
fn log_lost(thread: &str, consequence: &str, e: &dyn std::fmt::Display) {
    error!(
        error = format!("{e:#}"),
        "the X11 {thread} thread lost its connection; {consequence}"
    );
}

/// The error for a connection to the X display that has failed.
fn connection_failed(e: ConnectionError) -> anyhow::Error {
    anyhow::Error::new(e).context("the connection to the X display failed")
}

/// The error for an error the X server returned for one request.
fn x_error(e: &X11Error) -> anyhow::Error {
    anyhow!(
        "the X server returned {:?} for request {}.{}",
        e.error_kind,
        e.major_opcode,
        e.minor_opcode
    )
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
        Fault::Gone(connection_failed(e))
    }
}

impl From<ReplyError> for Fault {
    fn from(e: ReplyError) -> Self {
        match e {
            ReplyError::ConnectionError(e) => e.into(),
            ReplyError::X11Error(e) => Fault::Failed(x_error(&e)),
        }
    }
}

impl From<ReplyOrIdError> for Fault {
    fn from(e: ReplyOrIdError) -> Self {
        match e {
            ReplyOrIdError::ConnectionError(e) => e.into(),
            ReplyOrIdError::X11Error(e) => Fault::Failed(x_error(&e)),
            ReplyOrIdError::IdsExhausted => Fault::Failed(anyhow!(
                "the connection to the X display has run out of ids"
            )),
        }
    }
}

/// Send a request's result to whoever asked; a failed connection is
/// answered with [`Gone`]. Returns the error if the connection has
/// failed, after which the calling thread must stop.
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
            let _ = reply.send(Err(anyhow::Error::new(Gone(format!("{e:#}")))));
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
    // MIT-SHM 1.2 added ShmAttachFd.
    let shm_fd = present(xshm::X11_EXTENSION_NAME)? && {
        let version = xshm::query_version(conn)?
            .reply()
            .context("the MIT-SHM QueryVersion request")?;
        (version.major_version, version.minor_version) >= (1, 2)
    };
    Ok(ServerFacts {
        root_depth: screen.root_depth,
        bits_per_pixel,
        lsb_first: setup.image_byte_order == ImageOrder::LSB_FIRST,
        rgb_masks,
        xtest: present(xtest::X11_EXTENSION_NAME)?,
        xfixes: present(xfixes::X11_EXTENSION_NAME)?,
        xkb,
        damage: present(xdamage::X11_EXTENSION_NAME)?,
        shm_fd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::is_gone;

    #[test]
    fn a_dead_thread_makes_every_call_fail() {
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        drop(requests);
        let message = round_trip(&queue, "capture", CaptureRequest::Size)
            .unwrap_err()
            .to_string();
        assert!(message.contains("capture thread has stopped"), "{message}");
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        drop(requests);
        assert!(is_gone(
            &round_trip(&queue, "capture", CaptureRequest::Size).unwrap_err()
        ));

        // A thread that takes the request but dies before answering.
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        let worker = thread::spawn(move || drop(requests.recv()));
        assert!(
            round_trip(&queue, "capture", |reply| CaptureRequest::Capture(
                None, reply
            ))
            .is_err()
        );
        worker.join().unwrap();
    }

    #[test]
    fn a_failed_connection_stops_the_thread_but_answers_first() {
        let (reply, answered) = mpsc::sync_channel(1);
        let gone = answer::<()>(reply, Err(Fault::from(ConnectionError::UnknownError)));
        assert!(gone.is_some());
        assert!(is_gone(&answered.recv().unwrap().unwrap_err()));

        let (reply, answered) = mpsc::sync_channel(1);
        let gone = answer::<()>(reply, Err(Fault::Failed(anyhow!("BadMatch"))));
        assert!(
            gone.is_none(),
            "a refused request leaves the thread running"
        );
        assert!(!is_gone(&answered.recv().unwrap().unwrap_err()));
    }
}
