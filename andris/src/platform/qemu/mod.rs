//! The qemu platform: a guest's screen, taken from qemu's D-Bus display.
//!
//! qemu runs with `-display dbus,p2p=on`, which serves its display
//! interfaces to peers rather than on a bus, and a QMP socket. Andris
//! attaches in three steps:
//!
//! 1. Over QMP it passes qemu one end of a socket pair (`getfd`) and makes
//!    it a D-Bus display client (`add_client`, protocol `@dbus-display`).
//!    Andris holds the other end as a peer-to-peer D-Bus connection.
//! 2. On that connection it calls `RegisterListener` on the first
//!    console, handing over one end of a second socket pair.
//! 3. On the second pair it serves the listener ([`listener`]), which
//!    qemu calls with each change to the screen.
//!
//! The listener copies each change into a framebuffer of the platform's
//! own ([`framebuffer`]) and reports it to a [`DamageHub`]; captures read
//! from that copy. qemu reports exactly the rectangles the guest's device
//! reports: what a virtio-gpu guest flushes, and for std-vga whole
//! scanlines of what changed, polled every 30 ms.
//!
//! Only the picture crosses: this platform takes no input (events are
//! dropped), has no cursor shape, refuses to resize the guest, and has no
//! clipboard and no sound. It shows qemu's first console, and does not
//! take GPU (`gl=on`) scanouts.
//!
//! qemu's D-Bus calls run on zbus's own executor thread. A thread of the
//! platform's owns the QMP connection, and sets the [`GoneSignal`] when
//! qemu closes it, which it does when it exits.

mod framebuffer;
mod listener;
mod qmp;

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qemu_display::ConsoleProxy;
use tokio::sync::watch;
use tracing::{debug, info};
use zbus::proxy::CacheProperties;
use zbus::zvariant::Fd;

use super::{
    Capture, Clipboard, ClipboardReceiver, CursorReceiver, CursorShape, CursorSource, DamageHub,
    DamageSubscription, DisplayControl, DisplayGeometry, GoneSignal, InputEvent, InputSink,
    LockState, Platform, Rectangle, Resize, ScreenSource, Size,
};
pub use framebuffer::{A8R8G8B8, Framebuffer, PixelFormat, Source, X8R8G8B8};
use listener::{LISTENER_PATH, Listener, MapListener, Shared};
use qmp::Qmp;

/// How long each step of attaching may take: a QMP reply, the D-Bus
/// handshakes, and qemu's first picture of the screen.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// The name the display socket is passed to qemu under.
const DISPLAY_FD_NAME: &str = "andris-dbus-display";

/// The console andris shows: qemu's first.
const CONSOLE_PATH: &str = "/org/qemu/Display1/Console_0";

/// A guest's screen, from qemu's D-Bus display.
pub struct QemuPlatform {
    shared: Arc<Shared>,
    gone_signal: GoneSignal,
    cursor: watch::Sender<Option<Arc<CursorShape>>>,
    clipboard: watch::Sender<Option<Arc<str>>>,
}

impl QemuPlatform {
    /// Attach to the qemu whose QMP socket is at `qmp`, and wait for its
    /// first picture of the screen. Fails if qemu does not answer within
    /// [`ATTACH_TIMEOUT`] at any step, or refuses a D-Bus display client.
    pub fn connect(qmp: &Path) -> Result<Arc<Self>> {
        let shared = Arc::new(Shared::new());
        let (gone, gone_signal) = GoneSignal::new();
        let (attached_sender, attached) = std::sync::mpsc::channel();
        let thread_shared = shared.clone();
        let path = qmp.to_path_buf();
        std::thread::Builder::new()
            .name("andris-qemu".to_string())
            .spawn(move || match attach(&path, &thread_shared) {
                Err(error) => {
                    let _ = attached_sender.send(Err(error));
                }
                Ok((qmp, connections)) => {
                    let _ = attached_sender.send(Ok(()));
                    let why = qmp.wait_for_close();
                    drop(connections);
                    gone.set(why.clone());
                    let _ = thread_shared.damage.gone(why);
                }
            })
            .context("starting the qemu platform's thread")?;
        // Every step of attaching is bounded but the D-Bus handshakes, which
        // a qemu that stops answering could hold for ever.
        match attached.recv_timeout(ATTACH_TIMEOUT * 3) {
            Ok(result) => result?,
            Err(_) => bail!(
                "qemu did not take andris's display listener within {}s",
                (ATTACH_TIMEOUT * 3).as_secs()
            ),
        }
        let size = shared.wait_for_screen(ATTACH_TIMEOUT)?;
        info!("attached to qemu's D-Bus display; the guest's screen is {size}");
        Ok(Arc::new(Self {
            shared,
            gone_signal,
            cursor: watch::channel(None).0,
            clipboard: watch::channel(None).0,
        }))
    }

    /// The platform's seams, sharing this attachment.
    pub fn platform(self: &Arc<Self>) -> Platform {
        Platform {
            geometry: self.clone(),
            screen: self.clone(),
            input: self.clone(),
            cursor: self.clone(),
            display_control: self.clone(),
            clipboard: self.clone(),
            audio: None,
            gone: self.gone_signal.clone(),
        }
    }

    fn damage(&self) -> &DamageHub {
        &self.shared.damage
    }
}

/// The two D-Bus connections an attachment holds open: to qemu's display,
/// and the listener's.
struct Connections {
    _display: zbus::Connection,
    _listener: zbus::Connection,
}

/// Attach to qemu at `path`, as the module describes, with the listener
/// feeding `shared`.
fn attach(path: &Path, shared: &Arc<Shared>) -> Result<(Qmp, Connections)> {
    let mut qmp = Qmp::connect(path, ATTACH_TIMEOUT)?;
    let (display, theirs) = UnixStream::pair().context("making the display socket")?;
    qmp.getfd(DISPLAY_FD_NAME, theirs.as_fd())?;
    qmp.add_dbus_display_client(DISPLAY_FD_NAME)?;
    drop(theirs);
    debug!("qemu took the display socket");

    // zbus brings its own executor; this runtime only drives the
    // handshakes to completion on this thread.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the qemu platform's runtime")?;
    let connections = runtime.block_on(async {
        let display = zbus::connection::Builder::async_io_unix_stream(display)
            .p2p()
            .build()
            .await
            .context("opening qemu's D-Bus display")?;
        let console = ConsoleProxy::builder(&display)
            .path(CONSOLE_PATH)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .context("finding qemu's first console")?;

        // qemu answers RegisterListener before it opens the listener's
        // connection, and then waits, main loop held, for the handshake
        // that building the connection makes; both interfaces are served
        // from the start, since qemu asks for the shared screen at once.
        let (listener, theirs) = UnixStream::pair().context("making the listener's socket")?;
        console
            .register_listener(Fd::from(&theirs))
            .await
            .context("registering andris's display listener with qemu")?;
        drop(theirs);
        let listener = zbus::connection::Builder::async_io_unix_stream(listener)
            .p2p()
            .serve_at(LISTENER_PATH, Listener::new(shared.clone()))?
            .serve_at(LISTENER_PATH, MapListener::new(shared.clone()))?
            .build()
            .await
            .context("serving andris's display listener to qemu")?;
        anyhow::Ok(Connections {
            _display: display,
            _listener: listener,
        })
    })?;
    Ok((qmp, connections))
}

impl DisplayGeometry for QemuPlatform {
    fn size(&self) -> Result<Size> {
        self.shared
            .screen()?
            .as_ref()
            .map(Framebuffer::size)
            .ok_or_else(|| anyhow!("qemu has sent no screen"))
    }
}

impl ScreenSource for QemuPlatform {
    fn subscribe_damage(&self) -> Result<DamageSubscription> {
        self.damage().subscribe()
    }

    fn capture(&self, rects: &[Rectangle]) -> Result<Capture> {
        match self.shared.screen()?.as_ref() {
            Some(screen) => screen.capture(rects),
            None => bail!("qemu has sent no screen"),
        }
    }

    fn capture_all(&self) -> Result<Capture> {
        match self.shared.screen()?.as_ref() {
            Some(screen) => screen.capture_all(),
            None => bail!("qemu has sent no screen"),
        }
    }
}

impl InputSink for QemuPlatform {
    /// Drops the event: this platform takes no input.
    fn inject(&self, event: InputEvent) -> Result<()> {
        tracing::trace!(?event, "the qemu platform takes no input; dropped");
        Ok(())
    }

    /// No lock is on, as far as andris knows.
    fn lock_state(&self) -> Result<LockState> {
        Ok(LockState::default())
    }
}

impl CursorSource for QemuPlatform {
    /// Never has a shape.
    fn subscribe(&self) -> CursorReceiver {
        self.cursor.subscribe()
    }
}

impl DisplayControl for QemuPlatform {
    fn set_size(&self, _size: Size) -> Result<Resize> {
        Ok(Resize::Refused(
            "the qemu platform does not resize the guest's screen".to_string(),
        ))
    }
}

impl Clipboard for QemuPlatform {
    /// Never has text.
    fn changes(&self) -> ClipboardReceiver {
        self.clipboard.subscribe()
    }

    /// Drops the text: this platform has no clipboard.
    fn set(&self, _text: Arc<str>) -> Result<()> {
        Ok(())
    }
}
