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

mod capture;
mod checks;
mod cursor;
mod evdev_names;
mod input;
mod keymap;

use std::sync::Arc;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;

use anyhow::{Context, Result, anyhow};
use tokio::sync::watch;
use tracing::{error, info};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, EventMask, ImageOrder, VisualClass, Window,
};
use x11rb::protocol::{xfixes, xkb, xtest};
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::X11Error;

use super::{
    CursorReceiver, CursorSource, DisplayGeometry, Frame, FrameSource, Gone, InputEvent, InputSink,
    LockState, Platform, Size,
};
use capture::{CaptureRequest, serve_capture};
use checks::ServerFacts;
use cursor::{serve_cursor, start_cursor};
use input::{Injector, InputQueue, InputRequest, serve_input};

/// How many events [`InputSink::inject`] lets wait for the input thread.
pub const INPUT_QUEUE_LIMIT: usize = 8192;

/// How many more events [`InputSink::inject_release`] may queue beyond
/// [`INPUT_QUEUE_LIMIT`]. Ample for every key and button held at once.
pub const RELEASE_RESERVE: usize = 512;

/// The X11 platform. Create it with [`X11Platform::connect`] and hand the
/// server [`X11Platform::platform`].
pub struct X11Platform {
    capture: Sender<CaptureRequest>,
    input: InputQueue,
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
        let (input, input_queue) = InputQueue::new();
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
        assert!(round_trip(&queue, "capture", CaptureRequest::Capture).is_err());
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
