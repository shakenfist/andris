//! The platform against a fake qemu: a test that serves qemu's side of
//! the peer-to-peer display connection (the first console, with its
//! keyboard and mouse), takes the listener andris registers, shares a
//! screen with it, and records the input andris sends.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::mpsc::{self, Receiver};

use zbus::fdo;

use super::*;
use crate::platform::{Button, EvdevCode};

/// How long the test waits for anything andris or the fake does.
const WAIT: Duration = Duration::from_secs(10);

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// Padded, as a real scanout's rows may be.
const STRIDE: u32 = WIDTH * 4 + 32;

/// What the fake qemu's keyboard and mouse were asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    KeyPress(u32),
    KeyRelease(u32),
    ButtonPress(u32),
    ButtonRelease(u32),
    MoveTo(u32, u32),
    MoveBy(i32, i32),
}

/// `org.qemu.Display1.Console`: hands each listener andris registers to
/// the test.
struct FakeConsole {
    listeners: mpsc::Sender<OwnedFd>,
}

#[zbus::interface(name = "org.qemu.Display1.Console")]
impl FakeConsole {
    async fn register_listener(&self, listener: Fd<'_>) -> fdo::Result<()> {
        let listener = listener
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let _ = self.listeners.send(listener);
        Ok(())
    }
}

/// `org.qemu.Display1.Keyboard`, with Caps Lock and Num Lock on.
struct FakeKeyboard {
    calls: mpsc::Sender<Call>,
}

#[zbus::interface(name = "org.qemu.Display1.Keyboard")]
impl FakeKeyboard {
    async fn press(&self, keycode: u32) {
        let _ = self.calls.send(Call::KeyPress(keycode));
    }

    async fn release(&self, keycode: u32) {
        let _ = self.calls.send(Call::KeyRelease(keycode));
    }

    #[zbus(property)]
    fn modifiers(&self) -> u32 {
        // Num (1 << 1) and Caps (1 << 2).
        0b110
    }
}

/// `org.qemu.Display1.Mouse`, which is absolute.
struct FakeMouse {
    calls: mpsc::Sender<Call>,
}

#[zbus::interface(name = "org.qemu.Display1.Mouse")]
impl FakeMouse {
    async fn press(&self, button: u32) {
        let _ = self.calls.send(Call::ButtonPress(button));
    }

    async fn release(&self, button: u32) {
        let _ = self.calls.send(Call::ButtonRelease(button));
    }

    async fn set_abs_position(&self, x: u32, y: u32) {
        let _ = self.calls.send(Call::MoveTo(x, y));
    }

    async fn rel_motion(&self, dx: i32, dy: i32) -> fdo::Result<()> {
        let _ = self.calls.send(Call::MoveBy(dx, dy));
        Err(fdo::Error::Failed("Mouse is not relative".to_string()))
    }

    #[zbus(property)]
    fn is_absolute(&self) -> bool {
        true
    }
}

/// qemu's view of andris's listener.
#[zbus::proxy(
    interface = "org.qemu.Display1.Listener",
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/Listener",
    gen_blocking = false
)]
trait QemuListener {
    fn scanout(
        &self,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        data: &[u8],
    ) -> zbus::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn update(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        stride: u32,
        format: u32,
        data: &[u8],
    ) -> zbus::Result<()>;

    fn cursor_define(
        &self,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        data: &[u8],
    ) -> zbus::Result<()>;

    fn mouse_set(&self, x: i32, y: i32, on: i32) -> zbus::Result<()>;

    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<String>>;
}

/// qemu's view of andris's shared-memory listener.
#[zbus::proxy(
    interface = "org.qemu.Display1.Listener.Unix.Map",
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/Listener",
    gen_blocking = false
)]
trait QemuMap {
    fn scanout_map(
        &self,
        fd: Fd<'_>,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> zbus::Result<()>;

    fn update_map(&self, x: i32, y: i32, w: i32, h: i32) -> zbus::Result<()>;
}

/// The x8r8g8b8 bytes of pixel (`x`, `y`) in the screen's `generation`:
/// B, G and R vary with all three, and the unused byte is set, which
/// andris clears.
fn bgrx(x: u32, y: u32, generation: u8) -> [u8; 4] {
    [x as u8, y as u8, generation, 0xee]
}

/// Paint `rect` of the shared screen in `file` with `generation`.
fn paint(file: &std::fs::File, rect: Rectangle, generation: u8) {
    for y in rect.y..rect.y + rect.height {
        let row: Vec<u8> = (rect.x..rect.x + rect.width)
            .flat_map(|x| bgrx(x, y, generation))
            .collect();
        file.write_all_at(&row, u64::from(y * STRIDE + rect.x * 4))
            .unwrap();
    }
}

/// Pixel (`x`, `y`) of a capture of the whole screen.
fn captured(platform: &QemuPlatform, x: u32, y: u32) -> [u8; 4] {
    let frame = platform.capture_all().unwrap().into_whole().unwrap();
    let row = frame.row(y).unwrap();
    row[x as usize * 4..x as usize * 4 + 4].try_into().unwrap()
}

/// The next `count` calls the fake's devices took.
fn next_calls(calls: &Receiver<Call>, count: usize) -> Vec<Call> {
    (0..count)
        .map(|_| calls.recv_timeout(WAIT).unwrap())
        .collect()
}

#[test]
fn a_fake_qemu_shares_its_screen_and_takes_input() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let (listener_sender, listeners) = mpsc::channel();
    let (call_sender, calls) = mpsc::channel();
    // Connecting waits for the first screen, which the fake sends below.
    let connecting = std::thread::spawn(move || QemuPlatform::from_display(ours));

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async move {
        let display = zbus::connection::Builder::async_io_unix_stream(theirs)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .serve_at(
                CONSOLE_PATH,
                FakeConsole {
                    listeners: listener_sender,
                },
            )
            .unwrap()
            .serve_at(
                CONSOLE_PATH,
                FakeKeyboard {
                    calls: call_sender.clone(),
                },
            )
            .unwrap()
            .serve_at(CONSOLE_PATH, FakeMouse { calls: call_sender })
            .unwrap()
            .build()
            .await
            .unwrap();

        // qemu serves the listener's connection, as it does the display's.
        let listener = listeners.recv_timeout(WAIT).unwrap();
        let listener = zbus::connection::Builder::async_io_unix_stream(UnixStream::from(listener))
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .build()
            .await
            .unwrap();
        let plain = QemuListenerProxy::builder(&listener)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap();
        let map = QemuMapProxy::builder(&listener)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap();
        assert_eq!(
            plain.interfaces().await.unwrap(),
            vec!["org.qemu.Display1.Listener.Unix.Map".to_string()]
        );

        // A screen its file cannot hold is refused rather than mapped,
        // since reading past the end of a mapping would kill andris.
        let short = tempfile::tempfile().unwrap();
        short.set_len(u64::from(STRIDE * HEIGHT - 1)).unwrap();
        map.scanout_map(Fd::from(&short), 0, WIDTH, HEIGHT, STRIDE, X8R8G8B8)
            .await
            .unwrap_err();

        // Share the screen, which a capture then shows.
        let screen = tempfile::tempfile().unwrap();
        screen.set_len(u64::from(STRIDE * HEIGHT)).unwrap();
        let full = Rectangle::new(0, 0, WIDTH, HEIGHT);
        paint(&screen, full, 1);
        map.scanout_map(Fd::from(&screen), 0, WIDTH, HEIGHT, STRIDE, X8R8G8B8)
            .await
            .unwrap();
        let platform = connecting.join().unwrap().unwrap();
        assert_eq!(
            platform.size().unwrap(),
            Size {
                width: WIDTH,
                height: HEIGHT
            }
        );
        let damage = platform.subscribe_damage().unwrap();
        damage.take().unwrap();
        for (x, y) in [(0, 0), (WIDTH - 1, HEIGHT - 1), (17, 9)] {
            let [b, g, r, _] = bgrx(x, y, 1);
            assert_eq!(captured(&platform, x, y), [b, g, r, 0], "({x}, {y})");
        }

        // The guest draws everywhere but flushes one rectangle: only that
        // rectangle reaches andris's copy, and it is reported as damage.
        paint(&screen, full, 2);
        let flushed = Rectangle::new(8, 4, 16, 8);
        map.update_map(8, 4, 16, 8).await.unwrap();
        assert!(!damage.take().unwrap().is_empty());
        let [b, g, r, _] = bgrx(8, 4, 2);
        assert_eq!(captured(&platform, 8, 4), [b, g, r, 0]);
        let [b, g, r, _] = bgrx(23, 11, 2);
        assert_eq!(captured(&platform, 23, 11), [b, g, r, 0]);
        let [b, g, r, _] = bgrx(24, 11, 1);
        assert_eq!(captured(&platform, 24, 11), [b, g, r, 0]);
        let [b, g, r, _] = bgrx(8, 12, 1);
        assert_eq!(captured(&platform, 8, 12), [b, g, r, 0]);
        let patch = platform.capture(&[flushed]).unwrap();
        assert_eq!(patch.patches()[0].rect(), flushed);

        // A shared screen in a format andris refuses is refused to qemu,
        // which then sends pixels in the message, and ends the sharing:
        // later updates are not read from the screen shared before it.
        let error = map
            .scanout_map(Fd::from(&screen), 0, WIDTH, HEIGHT, STRIDE, 0x0804_0000)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("0x08040000"), "{error}");
        paint(&screen, full, 4);
        map.update_map(0, 0, 8, 8).await.unwrap();
        let [b, g, r, _] = bgrx(1, 1, 1);
        assert_eq!(captured(&platform, 1, 1), [b, g, r, 0]);
        // The picture comes back with the pixels qemu sends instead.
        let mut pixels = vec![0; (STRIDE * HEIGHT) as usize];
        screen.read_exact_at(&mut pixels, 0).unwrap();
        plain
            .scanout(WIDTH, HEIGHT, STRIDE, X8R8G8B8, &pixels)
            .await
            .unwrap();
        let [b, g, r, _] = bgrx(1, 1, 4);
        assert_eq!(captured(&platform, 1, 1), [b, g, r, 0]);

        // A screen shared from an offset that is not page aligned.
        let offset = 4099;
        let offset_screen = tempfile::tempfile().unwrap();
        offset_screen
            .set_len(u64::from(offset + STRIDE * HEIGHT))
            .unwrap();
        let mut rows = vec![0; (STRIDE * HEIGHT) as usize];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let at = (y * STRIDE + x * 4) as usize;
                rows[at..at + 4].copy_from_slice(&bgrx(x, y, 5));
            }
        }
        offset_screen
            .write_all_at(&rows, u64::from(offset))
            .unwrap();
        map.scanout_map(
            Fd::from(&offset_screen),
            offset,
            WIDTH,
            HEIGHT,
            STRIDE,
            X8R8G8B8,
        )
        .await
        .unwrap();
        let [b, g, r, _] = bgrx(WIDTH - 1, HEIGHT - 1, 5);
        assert_eq!(captured(&platform, WIDTH - 1, HEIGHT - 1), [b, g, r, 0]);
        // And an update of it, read at the offset too.
        let at = u64::from(offset + 3 * STRIDE + 2 * 4);
        offset_screen.write_all_at(&bgrx(2, 3, 6), at).unwrap();
        map.update_map(2, 3, 1, 1).await.unwrap();
        let [b, g, r, _] = bgrx(2, 3, 6);
        assert_eq!(captured(&platform, 2, 3), [b, g, r, 0]);
        let [b, g, r, _] = bgrx(3, 3, 5);
        assert_eq!(captured(&platform, 3, 3), [b, g, r, 0]);

        // Keys arrive as qnums, in order, a release included.
        platform
            .inject(InputEvent::Key {
                code: EvdevCode(30),
                pressed: true,
            })
            .unwrap();
        platform
            .inject_release(InputEvent::Key {
                code: EvdevCode(30),
                pressed: false,
            })
            .unwrap();
        // KEY_RIGHTCTRL, e0 1d.
        platform
            .inject(InputEvent::Key {
                code: EvdevCode(97),
                pressed: true,
            })
            .unwrap();
        // A key with no qnum is dropped, and the next still goes.
        platform
            .inject(InputEvent::Key {
                code: EvdevCode(700),
                pressed: true,
            })
            .unwrap();
        platform
            .inject(InputEvent::Key {
                code: EvdevCode(97),
                pressed: false,
            })
            .unwrap();
        assert_eq!(
            next_calls(&calls, 4),
            [
                Call::KeyPress(0x1e),
                Call::KeyRelease(0x1e),
                Call::KeyPress(0x9d),
                Call::KeyRelease(0x9d),
            ]
        );

        // The mouse is absolute, so offsets become positions, clamped to
        // the screen.
        for event in [
            InputEvent::PointerAbsolute { x: 10, y: 20 },
            InputEvent::PointerRelative { dx: 5, dy: -30 },
            InputEvent::Button {
                button: Button::Right,
                pressed: true,
            },
            InputEvent::Button {
                button: Button::Right,
                pressed: false,
            },
        ] {
            platform.inject(event).unwrap();
        }
        assert_eq!(
            next_calls(&calls, 4),
            [
                Call::MoveTo(10, 20),
                Call::MoveTo(15, 0),
                Call::ButtonPress(2),
                Call::ButtonRelease(2),
            ]
        );

        // The lock state is the keyboard's modifiers, read after the
        // events before it.
        assert_eq!(
            platform.lock_state().unwrap(),
            LockState {
                caps: true,
                num: true,
                scroll: false,
            }
        );
        assert!(calls.try_recv().is_err());

        // The cursor: a defined shape, hidden and shown by MouseSet, which
        // reports only a change of what is shown.
        let mut cursor = platform.subscribe();
        assert!(cursor.borrow_and_update().is_none());
        let words: [u32; 2] = [0xff11_2233, 0x8044_5566];
        let data: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
        plain.cursor_define(2, 1, 1, 0, &data).await.unwrap();
        tokio::time::timeout(WAIT, cursor.changed())
            .await
            .unwrap()
            .unwrap();
        let shape = cursor.borrow_and_update().clone().unwrap();
        assert_eq!(
            (shape.width(), shape.height(), shape.hot_x(), shape.hot_y()),
            (2, 1, 1, 0)
        );
        assert_eq!(shape.argb(), words);
        plain.mouse_set(3, 4, 1).await.unwrap();
        assert!(!cursor.has_changed().unwrap());
        plain.mouse_set(3, 4, 0).await.unwrap();
        assert!(cursor.has_changed().unwrap());
        assert!(cursor.borrow_and_update().is_none());
        plain.mouse_set(5, 6, 1).await.unwrap();
        let shown = cursor.borrow_and_update().clone().unwrap();
        assert!(Arc::ptr_eq(&shown, &shape));

        // A shape qemu cannot have meant is refused, and the last stays.
        plain.cursor_define(2, 2, 0, 0, &data).await.unwrap();
        assert!(!cursor.has_changed().unwrap());

        // qemu falls back to pixels in the message: a screen of another
        // size, which ends the sharing, then a patch with padded rows that
        // starts above the screen.
        let small: Vec<u8> = (0..8 * 4).flat_map(|i| [i as u8, 1, 2, 3]).collect();
        plain.scanout(8, 4, 32, X8R8G8B8, &small).await.unwrap();
        assert_eq!(
            platform.size().unwrap(),
            Size {
                width: 8,
                height: 4
            }
        );
        assert_eq!(captured(&platform, 5, 2), [21, 1, 2, 0]);
        let patch = [
            9, 9, 9, 9, 0xaa, 0xaa, 0xaa, 0xaa, 7, 7, 7, 7, 0xaa, 0xaa, 0xaa, 0xaa,
        ];
        plain
            .update(2, -1, 1, 2, 8, X8R8G8B8, &patch)
            .await
            .unwrap();
        assert_eq!(captured(&platform, 2, 0), [7, 7, 7, 0]);
        assert_eq!(captured(&platform, 2, 1), [10, 1, 2, 0]);
        // With the sharing over, a shared-screen update changes nothing.
        paint(&screen, full, 3);
        map.update_map(0, 0, 8, 4).await.unwrap();
        assert_eq!(captured(&platform, 5, 2), [21, 1, 2, 0]);

        // qemu exiting closes the display connection, and the platform
        // says the guest has gone.
        let mut gone = platform.platform().gone;
        assert!(gone.why().is_none());
        drop(display);
        let why = tokio::time::timeout(WAIT, gone.wait()).await.unwrap();
        assert!(why.contains("closed"), "{why}");
        assert!(crate::platform::is_gone(&damage.take().unwrap_err()));
    });
}
