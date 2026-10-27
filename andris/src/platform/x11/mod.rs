//! The X11 backend: captures an existing X display, injects input into
//! it, and follows its cursor and clipboard, through x11rb.
//!
//! # Threads
//!
//! x11rb's `RustConnection` is synchronous, so the backend opens five
//! connections to the display, each owned by its own thread:
//!
//! - **capture** answers size and capture requests with `GetGeometry` on
//!   the root window, and one image per rectangle asked for: with
//!   MIT-SHM's `ShmGetImage` into a memfd shared with the server where
//!   that works, and with `GetImage` otherwise (see [`capture`] and
//!   [`shm`]). Which is used, and why, is logged once at startup. It
//!   also makes the resizes a client asks for, with RandR (see
//!   [`randr`]), which are rare and answer in round trips;
//! - **input** injects events with XTest and reads the lock state from
//!   XKB's indicators. Both go through one queue, so a lock-state read
//!   sees every event queued before it;
//! - **cursor** blocks on XFixes cursor events, and publishes each new
//!   shape through a `watch` channel;
//! - **damage** blocks on XDamage events from a `RawRectangles` damage
//!   object on the root window, and on the root window's
//!   `ConfigureNotify`, and reports them to the platform's
//!   [`DamageHub`]: damage as damage, a new size as a resize (see
//!   [`damage`]);
//! - **clipboard** follows the `CLIPBOARD` selection's owner with XFixes,
//!   fetches each new owner's text and publishes it through a `watch`
//!   channel, and takes the selection to serve the text andris is given.
//!   It blocks on events and keeps a deadline on every transfer, so a
//!   program that never answers cannot stall it; a small timer thread
//!   wakes it when a deadline passes (see [`clipboard`]).
//!
//! Separate connections keep a key press from queueing behind a
//! multi-megabyte capture, and let the cursor, damage and clipboard
//! threads block on events while the capture thread serves round trips.
//! The threads talk to the rest of andris through `std::sync::mpsc`
//! queues, so [`InputSink::inject`] only enqueues and never blocks, as
//! [`Clipboard::set`] does.
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
//! A thread whose connection fails logs that once, sets the platform's
//! [`GoneSignal`] with the reason, and exits; every later call to it
//! returns an error. The first thread to find the connection dead gives
//! the reason the signal keeps. When the damage thread stops for any
//! reason but the platform's drop, the hub is told the desktop has
//! [`Gone`] too, so every [`DamageSubscription`] learns of it. An error
//! the X server returns for one request fails that request only.
//!
//! A call that waits on the capture or input thread warns once it has
//! waited [`STALL_WARNING`], and logs the answer when it comes (see
//! [`round_trip_warning`]): an X server that has stopped serving andris
//! is otherwise silent. The cursor, damage and clipboard threads wait
//! for events, which may rightly be slow to come, so they are not
//! watched.
//!
//! Dropping the platform closes the capture and input queues, and those
//! threads exit; the capture thread detaches its MIT-SHM segment as it
//! goes. The cursor, damage and clipboard threads are blocked waiting for
//! X events, so the drop also sends each a `ClientMessage` to an unmapped
//! window of its own, which wakes it to exit; the clipboard's timer
//! thread follows it. Any client of the display can find those windows
//! with `QueryTree` and send them a `ClientMessage` too, so the drop's
//! carries a random cookie, chosen once per process, and a thread stops
//! only for a message that carries it (see [`is_wake`]).
//!
//! # Keys
//!
//! Keys arrive as evdev codes, and become this server's keycodes by XKB
//! key name; see [`keymap`]. The server's names are read once, at
//! startup: a keymap changed later (with `setxkbmap`, say) is not seen.

mod capture;
mod checks;
mod clipboard;
mod cursor;
mod damage;
mod evdev_names;
mod input;
mod keymap;
mod randr;
// Unsafe code, as only this and the libpulse bindings are: the shared
// memory mapping. See the module's documentation for why it is sound.
#[allow(unsafe_code)]
mod shm;

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::watch;
use tracing::{error, info, warn};
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
    Capture, Clipboard, ClipboardReceiver, CursorReceiver, CursorSource, DamageHub,
    DamageSubscription, DisplayControl, DisplayGeometry, Gone, GoneSender, GoneSignal, InputEvent,
    InputSink, LockState, Platform, Rectangle, Resize, ScreenSource, Size,
};
use capture::{CaptureRequest, Capturer, serve_capture};
use checks::ServerFacts;
use clipboard::{ClipboardHandle, serve_clipboard, serve_timer, start_clipboard};
use cursor::{serve_cursor, start_cursor};
use damage::{DamageThread, serve_damage, start_damage};
use input::{Injector, InputQueue, InputRequest, serve_input};
use randr::Randr;

/// How many events [`InputSink::inject`] lets wait for the input thread.
pub const INPUT_QUEUE_LIMIT: usize = 8192;

/// How long a call to the capture or input thread waits for its answer
/// before andris warns that the X server may be stalled.
pub const STALL_WARNING: Duration = Duration::from_secs(5);

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

/// The cookie a wake message carries: random, and the same for every
/// waker in this process. Other clients of the display never see it,
/// because a `SendEvent` with an empty event mask goes only to the
/// client that created the window.
fn wake_cookie() -> [u32; 5] {
    static COOKIE: OnceLock<[u32; 5]> = OnceLock::new();
    *COOKIE.get_or_init(rand::random)
}

/// Whether `message`, sent to a thread's wake window, is the platform's
/// drop waking that thread to stop, rather than another client's.
fn is_wake(message: &ClientMessageEvent) -> bool {
    message.format == 32 && message.data.as_data32() == wake_cookie()
}

/// A thread blocked on X events, and how to wake it: a `ClientMessage`
/// carrying [`wake_cookie`] to a window of its own, sent on its own
/// connection.
struct Waker {
    conn: Arc<RustConnection>,
    window: Window,
}

impl Waker {
    /// Wake the thread. If its connection has gone the thread has already
    /// stopped, so a failure does not matter.
    fn wake(&self) {
        let event = ClientMessageEvent::new(32, self.window, AtomEnum::NONE, wake_cookie());
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
    /// Wakes the clipboard thread on drop.
    clipboard_waker: Waker,
    /// The desktop's damage, for every subscriber; the damage thread
    /// reports to it.
    damage: Arc<DamageHub>,
    /// Whether the capture thread is using MIT-SHM.
    using_shm: Arc<AtomicBool>,
    /// Set by whichever backend thread first finds its connection dead;
    /// each holds a sender.
    gone: GoneSignal,
    /// Queues text for the clipboard thread, and holds the desktop's text
    /// as it reports it.
    clipboard: ClipboardHandle,
}

impl X11Platform {
    /// Connect to X display `name` (`:10`, say), check that andris can
    /// export it, and start the backend's threads, with the default
    /// [`X11Options`].
    ///
    /// Fails, saying why, if the display cannot be reached, if its root
    /// window is not 32-bit BGRX, or if it lacks XTEST, XFIXES, XKEYBOARD
    /// or DAMAGE. Without RandR 1.2 it still connects, and refuses every
    /// resize.
    pub fn connect(name: &str) -> Result<Arc<Self>> {
        Self::connect_with(name, X11Options::default())
    }

    /// [`X11Platform::connect`], set up as `options` says.
    pub fn connect_with(name: &str, options: X11Options) -> Result<Arc<Self>> {
        // The input and capture connections first: the checks need the
        // input connection's XKB handshake, and the capture connection's
        // RandR handshake.
        let (input_conn, screen) = open(name, "input")?;
        let screen = input_conn
            .setup()
            .roots
            .get(screen)
            .cloned()
            .with_context(|| format!("X display {name} has no screen {screen}"))?;
        let (capture_conn, _) = open(name, "capture")?;
        let facts = server_facts(&input_conn, &capture_conn, &screen)
            .with_context(|| format!("asking X display {name} what it supports"))?;
        checks::check(name, &facts)?;
        let randr = match (checks::resize_refusal(&facts), facts.randr) {
            (None, Some(version)) => Ok(Randr::new(version)),
            (refusal, _) => {
                let why = refusal.unwrap_or_else(|| "the X server has no RandR".to_string());
                info!("andris will refuse to resize X display {name}, because {why}");
                Err(why)
            }
        };
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

        let unix = shm::is_unix_socket(capture_conn.stream().as_fd());
        let using_shm = Arc::new(AtomicBool::new(false));
        let capturer = Capturer::new(
            capture_conn,
            root,
            size,
            shm::refusal(options.shm, facts.shm_fd, unix),
            using_shm.clone(),
            randr,
        )
        .with_context(|| format!("setting up capture of X display {name}"))?;

        let (cursor_conn, _) = open(name, "the cursor")?;
        let cursor_conn = Arc::new(cursor_conn);
        let (cursor_window, first_shape) = start_cursor(&cursor_conn, root)
            .with_context(|| format!("following the cursor on X display {name}"))?;
        let (shapes, cursor) = watch::channel(first_shape.map(Arc::new));
        let (gone_sender, gone) = GoneSignal::new();

        let (clipboard_conn, _) = open(name, "the clipboard")?;
        let clipboard_conn = Arc::new(clipboard_conn);
        let started = start_clipboard(clipboard_conn.clone(), root, gone_sender.clone())
            .with_context(|| format!("following the clipboard on X display {name}"))?;

        let (capture, capture_queue) = mpsc::channel();
        let thread_gone = gone_sender.clone();
        spawn("andris-x11-capture", move || {
            serve_capture(capturer, capture_queue, thread_gone)
        })?;
        let (input, input_queue) = InputQueue::new();
        let thread_gone = gone_sender.clone();
        spawn("andris-x11-input", move || {
            serve_input(injector, input_queue, thread_gone)
        })?;
        let thread_conn = cursor_conn.clone();
        let thread_gone = gone_sender.clone();
        spawn("andris-x11-cursor", move || {
            serve_cursor(thread_conn, cursor_window, shapes, thread_gone)
        })?;
        let thread = DamageThread {
            conn: damage_conn.clone(),
            root,
            wake_window: damage_window,
            damage: damage_object,
            size,
            hub: damage.clone(),
            gone: gone_sender,
        };
        spawn("andris-x11-damage", move || serve_damage(thread))?;
        let clipboard_thread = started.thread;
        spawn("andris-x11-clipboard", move || {
            serve_clipboard(clipboard_thread)
        })?;
        let timer = started.timer;
        spawn("andris-x11-clipboard-timer", move || serve_timer(timer))?;

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
            clipboard_waker: Waker {
                conn: clipboard_conn,
                window: started.handle.window,
            },
            damage,
            using_shm,
            gone,
            clipboard: started.handle,
        }))
    }

    /// How the screen is being read. MIT-SHM can give way to `GetImage`
    /// after startup, if the desktop grows and the server refuses a
    /// larger segment, or if MIT-SHM captures keep failing.
    pub fn capture_method(&self) -> CaptureMethod {
        if self.using_shm.load(Ordering::Relaxed) {
            CaptureMethod::Shm
        } else {
            CaptureMethod::GetImage
        }
    }

    /// This platform as the six display seams, with no sound, and its gone
    /// signal.
    pub fn platform(self: &Arc<Self>) -> Platform {
        Platform {
            geometry: self.clone(),
            screen: self.clone(),
            input: self.clone(),
            cursor: self.clone(),
            display_control: self.clone(),
            clipboard: self.clone(),
            // Sound comes from the session's sound server, not the X
            // display: `andris` adds a PulseAudio source alongside.
            audio: None,
            gone: self.gone.clone(),
        }
    }
}

impl Drop for X11Platform {
    fn drop(&mut self) {
        // The capture and input threads stop when their queues close,
        // which happens as the fields drop. Wake the cursor, damage and
        // clipboard threads, which are waiting for X events.
        self.cursor_waker.wake();
        self.damage_waker.wake();
        self.clipboard_waker.wake();
    }
}

impl DisplayGeometry for X11Platform {
    fn size(&self) -> Result<Size> {
        round_trip(&self.capture, "capture", "size", CaptureRequest::Size)
    }
}

impl ScreenSource for X11Platform {
    fn subscribe_damage(&self) -> Result<DamageSubscription> {
        self.damage.subscribe()
    }

    fn capture(&self, rects: &[Rectangle]) -> Result<Capture> {
        round_trip(&self.capture, "capture", "capture", |reply| {
            CaptureRequest::Capture(Some(rects.to_vec()), reply)
        })
    }

    fn capture_all(&self) -> Result<Capture> {
        round_trip(&self.capture, "capture", "capture", |reply| {
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
        round_trip(
            &self.input.requests,
            "input",
            "lock state",
            InputRequest::LockState,
        )
    }
}

impl CursorSource for X11Platform {
    fn subscribe(&self) -> CursorReceiver {
        let mut receiver = self.cursor.clone();
        receiver.mark_unchanged();
        receiver
    }
}

impl DisplayControl for X11Platform {
    /// Resizes through RandR, on the capture thread; see [`randr`]. Every
    /// X error is a refusal, naming the error.
    fn set_size(&self, size: Size) -> Result<Resize> {
        round_trip(&self.capture, "capture", "resize", |reply| {
            CaptureRequest::SetSize(size, reply)
        })
    }
}

impl Clipboard for X11Platform {
    /// The text the clipboard thread fetched from the program that last
    /// took `CLIPBOARD`; see [`clipboard`].
    fn changes(&self) -> ClipboardReceiver {
        self.clipboard.changes()
    }

    /// Queues the text for the clipboard thread, which takes `CLIPBOARD`
    /// and serves it. Text over [`MAX_CLIPBOARD_TEXT`] is dropped, with a
    /// warning.
    ///
    /// [`MAX_CLIPBOARD_TEXT`]: super::MAX_CLIPBOARD_TEXT
    fn set(&self, text: Arc<str>) -> Result<()> {
        self.clipboard.set(text)
    }
}

/// The error for a call to a backend thread that has stopped.
fn stopped(thread: &str) -> anyhow::Error {
    anyhow::Error::new(Gone(format!(
        "the X11 {thread} thread has stopped, so the X display is no longer available"
    )))
}

/// Send `queue` the request `make` builds around a reply channel, and
/// wait for the answer, warning if it takes [`STALL_WARNING`] or more.
/// `what` names the request for the log.
fn round_trip<R, T>(
    queue: &Sender<R>,
    thread: &str,
    what: &str,
    make: impl FnOnce(SyncSender<Result<T>>) -> R,
) -> Result<T> {
    round_trip_warning(queue, thread, what, STALL_WARNING, make)
}

/// [`round_trip`], warning after `warn_after`.
///
/// Every request the capture and input threads answer is a round trip
/// to the X server, so an answer this late almost always means the X
/// server is not serving andris's connections: most often because
/// another X client holds a server grab. Nothing else would say so,
/// since the session just goes quiet. The wait goes on, as a grab can
/// lift by itself, and the answer is logged once it comes, as is the
/// thread stopping without one.
fn round_trip_warning<R, T>(
    queue: &Sender<R>,
    thread: &str,
    what: &str,
    warn_after: Duration,
    make: impl FnOnce(SyncSender<Result<T>>) -> R,
) -> Result<T> {
    let (reply, answer) = mpsc::sync_channel(1);
    let asked = Instant::now();
    queue.send(make(reply)).map_err(|_| stopped(thread))?;
    match answer.recv_timeout(warn_after) {
        Ok(result) => return result,
        Err(RecvTimeoutError::Disconnected) => return Err(stopped(thread)),
        Err(RecvTimeoutError::Timeout) => {}
    }
    warn!(
        thread,
        request = what,
        "the X11 {thread} thread has not answered in {:.1} s; the X server may be \
         stalled, perhaps by another X client holding a server grab",
        warn_after.as_secs_f64(),
    );
    let Ok(result) = answer.recv() else {
        warn!(
            thread,
            request = what,
            "the X11 {thread} thread stopped after {:.1} s without answering",
            asked.elapsed().as_secs_f64(),
        );
        return Err(stopped(thread));
    };
    info!(
        thread,
        request = what,
        "the X11 {thread} thread answered after {:.1} s",
        asked.elapsed().as_secs_f64(),
    );
    result
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
/// and is stopping, and what stops with it; then set `gone`, unless
/// another thread already has. Returns the reason given.
fn lost(gone: &GoneSender, thread: &str, consequence: &str, e: &dyn std::fmt::Display) -> String {
    error!(
        error = format!("{e:#}"),
        "the X11 {thread} thread lost its connection; {consequence}"
    );
    let why = format!("the X11 {thread} thread lost its connection to the X display: {e:#}");
    gone.set(why.clone());
    why
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
/// connection's first XKB request, and the RandR `QueryVersion` handshake
/// on `capture`, the connection that resizes.
fn server_facts(
    conn: &RustConnection,
    capture: &RustConnection,
    screen: &xproto::Screen,
) -> Result<ServerFacts> {
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
        randr: randr::handshake(capture)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::is_gone;
    use crate::test_logs::Logs;

    #[test]
    fn a_dead_thread_makes_every_call_fail() {
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        drop(requests);
        let message = round_trip(&queue, "capture", "size", CaptureRequest::Size)
            .unwrap_err()
            .to_string();
        assert!(message.contains("capture thread has stopped"), "{message}");
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        drop(requests);
        assert!(is_gone(
            &round_trip(&queue, "capture", "size", CaptureRequest::Size).unwrap_err()
        ));

        // A thread that takes the request but dies before answering. Its
        // death is no stall, so nothing is logged for it here.
        let logs = Logs::default();
        let _guard = logs.capture(tracing::Level::INFO);
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        let worker = thread::spawn(move || drop(requests.recv()));
        assert!(
            round_trip(&queue, "capture", "capture", |reply| {
                CaptureRequest::Capture(None, reply)
            })
            .is_err()
        );
        worker.join().unwrap();
        assert_eq!(logs.text(), "");
    }

    /// The seconds in a log line's "... after N s".
    fn seconds_after(line: &str) -> f64 {
        let (_, rest) = line.split_once(" after ").unwrap();
        rest.split_once(" s").unwrap().0.parse().unwrap()
    }

    /// A late answer is warned about once, while andris still waits, and
    /// logged again when it comes; a prompt one is not logged at all.
    /// The warning names the threshold, so only the answer's time varies,
    /// and it is checked as a lower bound: a busy machine only adds to it.
    #[test]
    fn a_late_answer_is_warned_about_and_its_arrival_logged() {
        let logs = Logs::default();
        let _guard = logs.capture(tracing::Level::INFO);
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        let worker = thread::spawn(move || {
            for (n, request) in requests.iter().enumerate() {
                let CaptureRequest::Size(reply) = request else {
                    panic!("not a size request");
                };
                if n == 1 {
                    thread::sleep(Duration::from_millis(300));
                }
                reply
                    .send(Ok(Size {
                        width: 8,
                        height: 8,
                    }))
                    .unwrap();
            }
        });
        let warn_after = Duration::from_millis(100);
        let patient = Duration::from_secs(60);
        round_trip_warning(&queue, "capture", "size", patient, CaptureRequest::Size).unwrap();
        assert_eq!(logs.text(), "", "a prompt answer was logged");

        let size = round_trip_warning(&queue, "capture", "size", warn_after, CaptureRequest::Size)
            .unwrap();
        assert_eq!(size.width, 8);
        let text = logs.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        for expected in [
            "WARN",
            "the X11 capture thread has not answered in 0.1 s",
            "another X client holding a server grab",
            "request=\"size\"",
        ] {
            assert!(
                lines[0].contains(expected),
                "{expected:?} missing from {}",
                lines[0]
            );
        }
        assert!(lines[1].contains("INFO"), "{}", lines[1]);
        assert!(
            lines[1].contains("the X11 capture thread answered after"),
            "{}",
            lines[1]
        );
        assert!(seconds_after(lines[1]) >= 0.3, "{}", lines[1]);
        drop(queue);
        worker.join().unwrap();
    }

    /// A thread that stops after the warning, without answering, fails
    /// the call as any stopped thread does, and says so in the log.
    #[test]
    fn a_thread_that_stops_after_the_warning_is_logged() {
        let logs = Logs::default();
        let _guard = logs.capture(tracing::Level::INFO);
        let (queue, requests) = mpsc::channel::<CaptureRequest>();
        let worker = thread::spawn(move || {
            let request = requests.recv().unwrap();
            thread::sleep(Duration::from_millis(300));
            drop(request);
        });
        let warn_after = Duration::from_millis(100);
        let error = round_trip_warning(&queue, "capture", "size", warn_after, CaptureRequest::Size)
            .unwrap_err();
        assert!(is_gone(&error), "{error:#}");
        worker.join().unwrap();

        let text = logs.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[0].contains("has not answered in 0.1 s"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("WARN"), "{}", lines[1]);
        assert!(
            lines[1].contains("the X11 capture thread stopped after"),
            "{}",
            lines[1]
        );
        assert!(lines[1].contains("without answering"), "{}", lines[1]);
        assert!(seconds_after(lines[1]) >= 0.3, "{}", lines[1]);
    }

    #[test]
    fn lost_sets_the_gone_signal_with_the_first_reason() {
        let (gone, signal) = GoneSignal::new();
        let why = lost(&gone, "capture", "capture stops", &"broken pipe");
        // scripts/e2e.sh looks for this wording in andris's log.
        assert_eq!(
            why,
            "the X11 capture thread lost its connection to the X display: broken pipe"
        );
        assert_eq!(signal.why().as_deref(), Some(why.as_str()));

        // A second thread losing its connection does not replace it.
        let later = lost(&gone, "input", "input stops", &"connection reset");
        assert!(later.contains("X11 input thread"), "{later}");
        assert_eq!(signal.why().as_deref(), Some(why.as_str()));
    }

    #[test]
    fn only_the_cookie_wakes_a_thread() {
        let window = 0x0040_0001;
        assert!(is_wake(&ClientMessageEvent::new(
            32,
            window,
            AtomEnum::NONE,
            wake_cookie()
        )));
        assert_eq!(wake_cookie(), wake_cookie(), "one cookie per process");
        assert!(!is_wake(&ClientMessageEvent::new(
            32,
            window,
            AtomEnum::NONE,
            [0u32; 5]
        )));
        let mut near = wake_cookie();
        near[4] ^= 1;
        assert!(!is_wake(&ClientMessageEvent::new(
            32,
            window,
            AtomEnum::NONE,
            near
        )));
        // The same bytes, sent as another format.
        let mut bytes = [0u8; 20];
        for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(wake_cookie()) {
            chunk.copy_from_slice(&word.to_ne_bytes());
        }
        assert!(!is_wake(&ClientMessageEvent::new(
            8,
            window,
            AtomEnum::NONE,
            bytes
        )));
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
