//! The X11 backend against a real X server: damage from a drawing, the
//! drawing captured through MIT-SHM and through `GetImage`, capture
//! carrying on after another client detaches andris's MIT-SHM segment,
//! resizes through RandR, and the clipboard both ways.
//!
//! The X server is named by `ANDRIS_TEST_DISPLAY`. `make e2e` sets it to
//! its Xvfb, before any window is mapped; with it unset, as under
//! `make test`, the test says on stderr that it is skipping, and passes
//! without doing anything. That is a pass that tested nothing, so the
//! lane that is meant to run these tests sets `ANDRIS_TEST_REQUIRED`,
//! and then a missing display fails instead of skipping: CI cannot go
//! green on these tests without having run them. The display must be
//! one nothing else draws on or attaches MIT-SHM segments to while the
//! test runs, and needs the
//! X-Resource extension, 1.2 or later. The tests take turns with the
//! display, and leave a filled rectangle on its root window.
//!
//! The resize test needs an X server that resizes up to its starting size
//! and no further, as Xvfb does: its `-screen` size is the largest RandR
//! allows. It leaves the display at that size.
//!
//! The clipboard tests copy and paste with `xclip`, which must be on the
//! `PATH`, and leave `CLIPBOARD` with no owner. One waits out andris's
//! 10-second conversion timeout.
//!
//! The input and cursor tests drive the real XTEST and XFixes paths: keys,
//! pointer motion, buttons and the XKB lock state injected and read back
//! through the X server, and a cursor set on the root window reported by
//! the cursor thread. They need a display no one else is typing at or
//! moving the pointer on, and they put back what they change: the keys
//! they press, the lock state and the root window's cursor.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use andris::platform::x11::{CaptureMethod, X11Options, X11Platform};
use andris::platform::{
    Button, Capture, Clipboard, ClipboardReceiver, CursorSource, DamageSubscription,
    DisplayControl, DisplayGeometry, EvdevCode, Frame, InputEvent, InputSink, LockState, Rectangle,
    Region, Resize, ScreenSource, Size, TILE, is_gone,
};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, SubwindowMode, Window,
    WindowClass,
};
use x11rb::protocol::{res, shm as xshm};
use x11rb::rust_connection::RustConnection;

/// Held by each test while it uses the display, which they cannot share:
/// one resizes it under the other's captures.
static DISPLAY_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A connection to the display that the tests keep open until the process
/// exits. An X server resets itself when its last client disconnects, and
/// refuses connections while it does; without this, the second test to
/// run can connect just as the first one's last connection closes.
static KEEP_OPEN: OnceLock<RustConnection> = OnceLock::new();

/// Where the test draws. It straddles a tile corner, at (128, 64), so
/// its damage spans four tiles.
const FILL_AREA: Rectangle = Rectangle::new(100, 50, 50, 40);

/// The colour it draws in, as a TrueColor pixel: red 0xff, green 0x80,
/// blue 0x40.
const FILL_PIXEL: u32 = 0x00ff_8040;

/// The same colour as BGRX bytes, without the unused fourth.
const FILL_BGR: [u8; 3] = [0x40, 0x80, 0xff];

/// How long damage may take to arrive.
const DAMAGE_BOUND: Duration = Duration::from_secs(5);

/// How long with no damage counts as the display having settled.
const SETTLED: Duration = Duration::from_millis(200);

/// The display to test against, or `None` to skip, saying so.
///
/// Panics instead of skipping when `ANDRIS_TEST_REQUIRED` is set, so a
/// lane that means to run these tests cannot pass by skipping them.
fn display() -> Option<String> {
    let display = std::env::var("ANDRIS_TEST_DISPLAY")
        .ok()
        .filter(|name| !name.is_empty());
    if let Some(name) = &display {
        KEEP_OPEN.get_or_init(|| x11rb::connect(Some(name)).unwrap().0);
    } else {
        assert!(
            !required(),
            "ANDRIS_TEST_REQUIRED is set but ANDRIS_TEST_DISPLAY is not: this lane is meant to \
             run the X11 backend's tests against a real X server, and skipping them here would \
             report success for code that never ran"
        );
        // Straight to stderr: libtest captures eprintln!, which would
        // hide that this passed without testing anything.
        let _ = writeln!(
            std::io::stderr(),
            "ANDRIS_TEST_DISPLAY is unset; SKIPPING the X11 backend test, which `make e2e` runs"
        );
    }
    display
}

/// Whether the environment says these tests must run rather than skip.
fn required() -> bool {
    std::env::var("ANDRIS_TEST_REQUIRED").is_ok_and(|value| !value.is_empty())
}

/// Fill `rect` of `display`'s root window with `pixel`, through a
/// connection of its own, and wait until the server has done it.
fn draw(display: &str, rect: Rectangle, pixel: u32) {
    let (conn, screen) = x11rb::connect(Some(display)).unwrap();
    let root = conn.setup().roots[screen].root;
    let gc = conn.generate_id().unwrap();
    conn.create_gc(
        gc,
        root,
        &CreateGCAux::new()
            .foreground(pixel)
            .subwindow_mode(SubwindowMode::INCLUDE_INFERIORS),
    )
    .unwrap();
    conn.poly_fill_rectangle(
        root,
        gc,
        &[xproto::Rectangle {
            x: rect.x.try_into().unwrap(),
            y: rect.y.try_into().unwrap(),
            width: rect.width.try_into().unwrap(),
            height: rect.height.try_into().unwrap(),
        }],
    )
    .unwrap();
    conn.free_gc(gc).unwrap();
    // A round trip, so the fill is done before this returns.
    conn.get_input_focus().unwrap().reply().unwrap();
}

/// Whether `region` holds every tile `rect` touches.
fn covers(region: &Region, rect: Rectangle) -> bool {
    let cols = rect.x / TILE..=(rect.x + rect.width - 1) / TILE;
    let rows = rect.y / TILE..=(rect.y + rect.height - 1) / TILE;
    rows.into_iter()
        .all(|row| cols.clone().all(|col| region.contains_tile(col, row)))
}

/// Take damage from `subscription` until it covers `rect`, and return
/// all that was taken. Panics after [`DAMAGE_BOUND`].
async fn damage_covering(subscription: &DamageSubscription, rect: Rectangle) -> Region {
    let wait = async {
        let mut seen: Option<Region> = None;
        loop {
            subscription.changed().await;
            let taken = subscription.take().unwrap();
            let seen = seen.get_or_insert_with(|| Region::empty(taken.size()));
            seen.union(&taken);
            if covers(seen, rect) {
                return seen.clone();
            }
        }
    };
    tokio::time::timeout(DAMAGE_BOUND, wait)
        .await
        .unwrap_or_else(|_| panic!("no damage covering {rect} within {DAMAGE_BOUND:?}"))
}

/// Take damage from `subscription` until none arrives for [`SETTLED`].
/// The X server reports a damage object's whole window when it is
/// created, and that report may land after a subscription's first take.
async fn settle(subscription: &DamageSubscription) {
    while tokio::time::timeout(SETTLED, subscription.changed())
        .await
        .is_ok()
    {
        subscription.take().unwrap();
    }
}

/// A frame's pixels without the unused fourth byte, row by row.
fn bgr(frame: &Frame) -> Vec<u8> {
    (0..frame.height())
        .flat_map(|y| frame.row(y).unwrap().as_chunks::<4>().0)
        .flat_map(|pixel| pixel[..3].to_vec())
        .collect()
}

/// A capture as its size and, for each patch, its rectangle and pixels
/// without the unused byte, for comparing two captures.
fn summary(capture: &Capture) -> (String, Vec<(Rectangle, Vec<u8>)>) {
    (
        capture.size().to_string(),
        capture
            .patches()
            .iter()
            .map(|patch| (patch.rect(), bgr(patch.pixels())))
            .collect(),
    )
}

/// Capture `rects` and the whole desktop through `platform`.
fn captures(platform: &Arc<X11Platform>, rects: &[Rectangle]) -> (Capture, Capture) {
    (
        platform.capture(rects).unwrap(),
        platform.capture_all().unwrap(),
    )
}

/// Every resource on the display, as its XID and its type's name, found
/// as any other client of the display could: with X-Resource 1.2's
/// `QueryResourceBytes`, for any resource of any client.
fn resources(conn: &RustConnection) -> Vec<(u32, String)> {
    let version = res::query_version(conn, 1, 2).unwrap().reply().unwrap();
    assert!(
        (version.server_major, version.server_minor) >= (1, 2),
        "QueryResourceBytes needs X-Resource 1.2"
    );
    let any = res::ResourceIdSpec {
        resource: 0,
        type_: 0,
    };
    res::query_resource_bytes(conn, 0, &[any])
        .unwrap()
        .reply()
        .unwrap()
        .sizes
        .iter()
        .map(|value| {
            let spec = value.size.spec;
            let name = conn
                .get_atom_name(spec.type_)
                .unwrap()
                .reply()
                .unwrap()
                .name;
            (spec.resource, String::from_utf8_lossy(&name).into_owned())
        })
        .collect()
}

/// The XIDs of every MIT-SHM segment attached on the display.
fn shm_segments(conn: &RustConnection) -> Vec<u32> {
    resources(conn)
        .into_iter()
        .filter(|(_, kind)| kind == "ShmSeg")
        .map(|(id, _)| id)
        .collect()
}

/// Detach, from `conn`, the one MIT-SHM segment attached on the display,
/// which is andris's.
fn detach_the_segment(conn: &RustConnection) {
    let [segment] = shm_segments(conn)[..] else {
        let mut kinds: Vec<String> = resources(conn).into_iter().map(|(_, kind)| kind).collect();
        kinds.sort();
        kinds.dedup();
        panic!(
            "expected one MIT-SHM segment, andris's, among resources of types {kinds:?}: {:?}",
            shm_segments(conn)
        );
    };
    xshm::detach(conn, segment).unwrap().check().unwrap();
}

/// Capture `FILL_AREA` through `platform` until a capture works, checking
/// that it matches `expected`. Returns how many failed first; panics if
/// any failure is the desktop going, or if capture never recovers.
fn failures_before_recovery(platform: &Arc<X11Platform>, expected: &Capture) -> usize {
    for failed in 0..10 {
        match platform.capture(&[FILL_AREA]) {
            Ok(capture) => {
                assert_eq!(summary(&capture), summary(expected));
                return failed;
            }
            Err(e) => assert!(!is_gone(&e), "a dead segment is not a dead display: {e:#}"),
        }
    }
    panic!("capture did not recover from a detached MIT-SHM segment");
}

#[tokio::test]
async fn damage_and_both_capture_paths_see_a_drawing() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let shm = X11Platform::connect(&display).unwrap();
    assert_eq!(
        shm.capture_method(),
        CaptureMethod::Shm,
        "MIT-SHM should work on a local Xvfb"
    );
    let size = shm.size().unwrap();
    let subscription = shm.subscribe_damage().unwrap();
    assert_eq!(
        subscription.take().unwrap(),
        Region::full(size),
        "a new subscription starts with the whole desktop"
    );
    settle(&subscription).await;

    draw(&display, FILL_AREA, FILL_PIXEL);
    let damage = damage_covering(&subscription, FILL_AREA).await;
    assert_ne!(
        damage,
        Region::full(size),
        "a small drawing does not damage the whole desktop"
    );

    let capture = shm.capture(&[FILL_AREA]).unwrap();
    assert_eq!(capture.size(), size);
    let [patch] = capture.patches() else {
        panic!("one rectangle, one patch: {capture:?}");
    };
    assert_eq!(patch.rect(), FILL_AREA);
    assert!(
        bgr(patch.pixels())
            .as_chunks::<3>()
            .0
            .iter()
            .all(|pixel| *pixel == FILL_BGR),
        "every captured pixel is the fill colour"
    );

    // Rectangles that straddle the fill, cross the bottom right corner
    // (clipped), lie wholly outside (dropped), and add up to more than
    // the segment holds (two batches), compared across both paths.
    let rects = [
        Rectangle::new(90, 40, 70, 60),
        Rectangle::new(size.width - 10, size.height - 5, 64, 64),
        Rectangle::new(size.width + 5, 0, 5, 5),
        Rectangle::full(size),
        Rectangle::new(0, 0, 64, 64),
    ];
    let (shm_rects, shm_all) = captures(&shm, &rects);
    assert_eq!(
        shm_rects
            .patches()
            .iter()
            .map(|patch| patch.rect())
            .collect::<Vec<_>>(),
        vec![
            rects[0],
            Rectangle::new(size.width - 10, size.height - 5, 10, 5),
            rects[3],
            rects[4],
        ]
    );

    let get_image = X11Platform::connect_with(
        &display,
        X11Options {
            shm: false,
            ..X11Options::default()
        },
    )
    .unwrap();
    assert_eq!(get_image.capture_method(), CaptureMethod::GetImage);
    let (get_image_rects, get_image_all) = captures(&get_image, &rects);
    assert_eq!(summary(&shm_rects), summary(&get_image_rects));
    assert_eq!(summary(&shm_all), summary(&get_image_all));
    let fallback = get_image.capture(&[FILL_AREA]).unwrap();
    assert_eq!(summary(&fallback), summary(&capture));
    drop(get_image);

    // Another client detaches andris's segment. MIT-SHM captures then
    // fail until andris attaches a fresh segment, and carry on with it.
    let (other, _) = x11rb::connect(Some(display.as_str())).unwrap();
    detach_the_segment(&other);
    assert!(failures_before_recovery(&shm, &capture) > 0);
    assert_eq!(
        shm.capture_method(),
        CaptureMethod::Shm,
        "a fresh segment replaced the detached one"
    );
    assert_eq!(shm_segments(&other).len(), 1);

    // Detached again, andris gives MIT-SHM up for GetImage.
    detach_the_segment(&other);
    assert!(failures_before_recovery(&shm, &capture) > 0);
    assert_eq!(shm.capture_method(), CaptureMethod::GetImage);
    assert!(shm_segments(&other).is_empty());
    let (after_rects, after_all) = captures(&shm, &rects);
    assert_eq!(summary(&after_rects), summary(&shm_rects));
    assert_eq!(summary(&after_all), summary(&shm_all));
    eprintln!(
        "damage, MIT-SHM and GetImage agree on X display {display}, and capture survives a \
         detached segment"
    );
}

/// Take damage from `subscription` until it reports a desktop of `size`,
/// as a resize does. Panics after [`DAMAGE_BOUND`].
async fn resized_to(subscription: &DamageSubscription, size: Size) {
    let wait = async {
        loop {
            subscription.changed().await;
            if subscription.take().unwrap().size() == size {
                return;
            }
        }
    };
    tokio::time::timeout(DAMAGE_BOUND, wait)
        .await
        .unwrap_or_else(|_| panic!("no resize to {size} reported within {DAMAGE_BOUND:?}"));
}

/// The names of andris's modes on the display: every one the server has,
/// and every one an output lists, each sorted.
fn andris_modes(conn: &RustConnection, root: Window) -> (Vec<String>, Vec<String>) {
    let resources = conn
        .randr_get_screen_resources_current(root)
        .unwrap()
        .reply()
        .unwrap();
    let mut names = resources.names.as_slice();
    let mut all = Vec::new();
    for mode in &resources.modes {
        let (name, rest) = names.split_at(mode.name_len.into());
        names = rest;
        all.push((mode.id, String::from_utf8_lossy(name).into_owned()));
    }
    let ours = |ids: &mut dyn Iterator<Item = u32>| -> Vec<String> {
        let mut found: Vec<String> = ids
            .filter_map(|id| all.iter().find(|(mode, _)| *mode == id))
            .map(|(_, name)| name.clone())
            .filter(|name| name.starts_with("andris-"))
            .collect();
        found.sort();
        found
    };
    let on_server = ours(&mut all.iter().map(|(id, _)| *id));
    let mut listed = Vec::new();
    for &output in &resources.outputs {
        let info = conn
            .randr_get_output_info(output, resources.config_timestamp)
            .unwrap()
            .reply()
            .unwrap();
        listed.extend(info.modes);
    }
    (on_server, ours(&mut listed.into_iter()))
}

/// Resize `platform` to `size`, expecting it to apply, and check that the
/// desktop and a capture are then that size, that damage subscribers hear
/// of it, and that the only andris modes are `modes`.
async fn applied(
    platform: &Arc<X11Platform>,
    subscription: &DamageSubscription,
    conn: &RustConnection,
    root: Window,
    asked: Size,
    size: Size,
    modes: &[&str],
) {
    assert_eq!(
        platform.set_size(asked).unwrap(),
        Resize::Applied,
        "resizing to {asked}"
    );
    assert_eq!(platform.size().unwrap(), size, "resizing to {asked}");
    resized_to(subscription, size).await;
    assert_eq!(platform.capture_all().unwrap().size(), size);
    let modes: Vec<String> = modes.iter().map(|mode| mode.to_string()).collect();
    assert_eq!(
        andris_modes(conn, root),
        (modes.clone(), modes),
        "andris's modes after resizing to {asked}"
    );
}

#[tokio::test]
async fn randr_resizes_up_to_the_start_size_and_cleans_up_its_modes() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let platform = X11Platform::connect(&display).unwrap();
    let start = platform.size().unwrap();
    let (other, screen) = x11rb::connect(Some(display.as_str())).unwrap();
    let root = other.setup().roots[screen].root;
    other.randr_query_version(1, 3).unwrap().reply().unwrap();
    let range = other
        .randr_get_screen_size_range(root)
        .unwrap()
        .reply()
        .unwrap();
    assert_eq!(
        Size {
            width: range.max_width.into(),
            height: range.max_height.into()
        },
        start,
        "the X server's largest screen is its starting size, as Xvfb's is"
    );
    assert_eq!(andris_modes(&other, root), (vec![], vec![]));
    let subscription = platform.subscribe_damage().unwrap();
    settle(&subscription).await;

    // Shrinking: a mode is created for the size, and stays while in use.
    let small = Size {
        width: 640,
        height: 480,
    };
    applied(
        &platform,
        &subscription,
        &other,
        root,
        small,
        small,
        &["andris-640x480"],
    )
    .await;
    assert_eq!(platform.set_size(small).unwrap(), Resize::Unchanged);

    // Wider than the server allows: clamped to its widest, and applied.
    // The mode no longer in use goes.
    let wide = Size {
        width: start.width,
        height: 600,
    };
    applied(
        &platform,
        &subscription,
        &other,
        root,
        Size {
            width: start.width + 1000,
            height: 600,
        },
        wide,
        &[&format!("andris-{wide}")],
    )
    .await;

    // Narrower but taller: the screen grows to hold both CRTCs first.
    let tall = Size {
        width: 800,
        height: start.height,
    };
    applied(
        &platform,
        &subscription,
        &other,
        root,
        tall,
        tall,
        &[&format!("andris-{tall}")],
    )
    .await;

    // Back to the start, whose mode is the server's own, so andris's go.
    applied(&platform, &subscription, &other, root, start, start, &[]).await;

    // Larger than the server allows, at its largest already: nothing can
    // change, and the client did not get what it asked for.
    let large = Size {
        width: start.width + 256,
        height: start.height + 256,
    };
    let Resize::Refused(why) = platform.set_size(large).unwrap() else {
        panic!("a resize beyond the server's largest, at its largest, is refused");
    };
    assert!(why.contains(&start.to_string()), "{why}");
    assert_eq!(platform.size().unwrap(), start);
    assert_eq!(andris_modes(&other, root), (vec![], vec![]));
    eprintln!("RandR resized X display {display} within its size range, and refused beyond it");
}

/// How long the clipboard may take to report a copy, or to serve a paste.
const CLIPBOARD_BOUND: Duration = Duration::from_secs(5);

/// How long andris gives an owner to answer a conversion.
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// A program that copied text to `CLIPBOARD` with `xclip`, offering it
/// as `target` if given and as `UTF8_STRING` if not. It serves the text
/// until another program takes the selection, or it is dropped.
struct Copier(Child);

impl Copier {
    fn new(display: &str, target: Option<&str>, bytes: &[u8]) -> Self {
        let mut command = Command::new("xclip");
        // -quiet keeps xclip in the foreground, where it can be killed.
        command.args([
            "-display",
            display,
            "-selection",
            "clipboard",
            "-i",
            "-quiet",
        ]);
        if let Some(target) = target {
            command.args(["-t", target]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        // Dropping stdin closes it, and xclip then takes the selection.
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        Self(child)
    }
}

impl Drop for Copier {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Paste `CLIPBOARD` from `display` as `target` with `xclip`, or `None`
/// if the owner refuses, or there is none.
fn paste(display: &str, target: &str) -> Option<Vec<u8>> {
    let output = Command::new("timeout")
        .args(["5", "xclip", "-display", display, "-selection", "clipboard"])
        .args(["-o", "-t", target])
        .stderr(Stdio::null())
        .output()
        .unwrap();
    output.status.success().then_some(output.stdout)
}

/// Paste until `CLIPBOARD`, as `UTF8_STRING`, is `text`, which text andris
/// was just given takes a moment to become. Panics after
/// [`CLIPBOARD_BOUND`].
fn pasted_eventually(display: &str, text: &str) {
    let start = Instant::now();
    loop {
        let pasted = paste(display, "UTF8_STRING");
        if pasted.as_deref() == Some(text.as_bytes()) {
            return;
        }
        assert!(
            start.elapsed() < CLIPBOARD_BOUND,
            "pasting {} bytes: got {:?} bytes",
            text.len(),
            pasted.map(|bytes| bytes.len())
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The target names `CLIPBOARD`'s owner offers, sorted.
fn targets(display: &str) -> Vec<String> {
    let mut names: Vec<String> = String::from_utf8(paste(display, "TARGETS").unwrap())
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

/// The next text the clipboard reports. Panics after [`CLIPBOARD_BOUND`].
async fn reported(changes: &mut ClipboardReceiver) -> Option<Arc<str>> {
    tokio::time::timeout(CLIPBOARD_BOUND, changes.changed())
        .await
        .expect("no clipboard change within the bound")
        .unwrap();
    changes.borrow_and_update().clone()
}

/// Text of about `bytes` bytes, many characters of them two bytes long,
/// and different on every line.
fn big_text(bytes: usize) -> String {
    let mut text = String::with_capacity(bytes + 64);
    let mut line = 0;
    while text.len() < bytes {
        text.push_str(&format!("line {line}: ünïcödé ‘text’ — {}\n", line * 7));
        line += 1;
    }
    text
}

/// A connection, a window on it, and an atom, for a test client.
fn client(display: &str, atom: &str) -> (x11rb::rust_connection::RustConnection, Window, u32) {
    let (conn, screen) = x11rb::connect(Some(display)).unwrap();
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
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
    )
    .unwrap();
    let atom = conn
        .intern_atom(false, atom.as_bytes())
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    (conn, window, atom)
}

/// Take `CLIPBOARD` from a client that never reads its events, so never
/// answers a request for the text. It owns the selection until dropped.
fn squat(display: &str) -> x11rb::rust_connection::RustConnection {
    let (conn, window, clipboard) = client(display, "CLIPBOARD");
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
        .unwrap();
    let owner = conn
        .get_selection_owner(clipboard)
        .unwrap()
        .reply()
        .unwrap()
        .owner;
    assert_eq!(owner, window);
    conn
}

/// Ask for `CLIPBOARD` as `UTF8_STRING` from a client that never takes
/// more than the first answer, and return that answer's type's name with
/// the client, which leaves an INCR transfer stalled while it lives.
fn stall_a_transfer(display: &str) -> (x11rb::rust_connection::RustConnection, String) {
    let (conn, window, clipboard) = client(display, "CLIPBOARD");
    let utf8 = conn
        .intern_atom(false, b"UTF8_STRING")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let property = conn
        .intern_atom(false, b"STALLED")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    conn.convert_selection(window, clipboard, utf8, property, x11rb::CURRENT_TIME)
        .unwrap();
    conn.flush().unwrap();
    let start = Instant::now();
    loop {
        if let Some(Event::SelectionNotify(event)) = conn.poll_for_event().unwrap() {
            assert_eq!(event.property, property, "the conversion was refused");
            break;
        }
        assert!(
            start.elapsed() < CLIPBOARD_BOUND,
            "no answer to a conversion"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let answer = conn
        .get_property(false, window, property, AtomEnum::ANY, 0, 1)
        .unwrap()
        .reply()
        .unwrap();
    let name = conn
        .get_atom_name(answer.type_)
        .unwrap()
        .reply()
        .unwrap()
        .name;
    (conn, String::from_utf8(name).unwrap())
}

#[tokio::test]
async fn copies_on_the_display_are_reported() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let platform = X11Platform::connect(&display).unwrap();
    let mut changes = platform.changes();

    let _first = Copier::new(&display, None, b"first copy");
    assert_eq!(reported(&mut changes).await.as_deref(), Some("first copy"));
    let _again = Copier::new(&display, None, b"first copy");
    assert_eq!(
        reported(&mut changes).await.as_deref(),
        Some("first copy"),
        "a copy of the same text is reported again"
    );

    let wide = "héllo wörld — 世界 🎉";
    let _wide = Copier::new(&display, None, wide.as_bytes());
    assert_eq!(reported(&mut changes).await.as_deref(), Some(wide));

    // xclip sends anything over a megabyte by INCR.
    let big = big_text(3 * 1024 * 1024);
    let _big = Copier::new(&display, None, big.as_bytes());
    let got = reported(&mut changes).await.unwrap();
    assert_eq!(got.len(), big.len());
    assert!(*got == *big, "the INCR transfer's text differs");

    // An owner offering only Latin-1.
    let latin1 = Copier::new(&display, Some("STRING"), b"caf\xe9 cr\xe8me");
    assert_eq!(reported(&mut changes).await.as_deref(), Some("café crème"));

    // The owner goes, and the clipboard is empty.
    drop(latin1);
    assert_eq!(reported(&mut changes).await, None);
    eprintln!("copies on X display {display}, large and Latin-1 among them, reached andris");
}

#[tokio::test]
async fn text_set_through_the_seam_is_pasted() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let platform = X11Platform::connect(&display).unwrap();
    let changes = platform.changes();

    platform.set(Arc::from("café")).unwrap();
    pasted_eventually(&display, "café");
    assert_eq!(paste(&display, "STRING").unwrap(), b"caf\xe9");
    assert_eq!(
        paste(&display, "text/plain;charset=utf-8").unwrap(),
        "café".as_bytes()
    );
    assert_eq!(
        targets(&display),
        [
            "STRING",
            "TARGETS",
            "TIMESTAMP",
            "UTF8_STRING",
            "text/plain;charset=utf-8"
        ]
    );
    assert_eq!(paste(&display, "MULTIPLE"), None);
    assert_eq!(paste(&display, "image/png"), None);

    // Text Latin-1 cannot carry is not offered as STRING.
    let wide = "世界 🎉 — wide";
    platform.set(Arc::from(wide)).unwrap();
    pasted_eventually(&display, wide);
    assert!(!targets(&display).contains(&"STRING".to_string()));
    assert_eq!(paste(&display, "STRING"), None);

    // Over a megabyte goes by INCR, and a requestor that stops taking
    // pieces does not hold up the next paste.
    let big = big_text(3 * 1024 * 1024);
    platform.set(Arc::from(big.as_str())).unwrap();
    pasted_eventually(&display, &big);
    let (_stalled, kind) = stall_a_transfer(&display);
    assert_eq!(kind, "INCR");
    assert!(paste(&display, "UTF8_STRING").unwrap() == big.as_bytes());

    // A request naming andris's own window as the requestor is ignored:
    // andris writes nothing there, and goes on serving.
    let (conn, _, clipboard) = client(&display, "CLIPBOARD");
    let owner = conn
        .get_selection_owner(clipboard)
        .unwrap()
        .reply()
        .unwrap()
        .owner;
    let utf8 = conn
        .intern_atom(false, b"UTF8_STRING")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let property = conn
        .intern_atom(false, b"HIJACKED")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    conn.convert_selection(owner, clipboard, utf8, property, x11rb::CURRENT_TIME)
        .unwrap();
    // A round trip, so the server has passed the request on before the
    // paste below asks for the text.
    conn.get_input_focus().unwrap().reply().unwrap();
    assert!(paste(&display, "UTF8_STRING").unwrap() == big.as_bytes());
    let left = conn
        .get_property(false, owner, property, AtomEnum::ANY, 0, 1)
        .unwrap()
        .reply()
        .unwrap();
    assert_eq!(left.type_, x11rb::NONE, "andris wrote to its own window");

    assert!(
        !changes.has_changed().unwrap(),
        "text andris sets is not reported back"
    );
    eprintln!("text set through andris pasted on X display {display}, large and wide among it");
}

#[tokio::test]
async fn another_owner_takes_the_clipboard_from_andris() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let platform = X11Platform::connect(&display).unwrap();
    let mut changes = platform.changes();

    platform.set(Arc::from("andris's text")).unwrap();
    pasted_eventually(&display, "andris's text");
    let _theirs = Copier::new(&display, None, b"their text");
    assert_eq!(reported(&mut changes).await.as_deref(), Some("their text"));
    assert_eq!(paste(&display, "UTF8_STRING").unwrap(), b"their text");

    // Andris taking it back withdraws their text.
    platform.set(Arc::from("andris's text again")).unwrap();
    assert_eq!(reported(&mut changes).await, None);
    pasted_eventually(&display, "andris's text again");

    // Once they take it again and go, nobody serves the clipboard:
    // andris stopped.
    let theirs = Copier::new(&display, None, b"their text again");
    assert_eq!(
        reported(&mut changes).await.as_deref(),
        Some("their text again")
    );
    drop(theirs);
    assert_eq!(reported(&mut changes).await, None);
    assert_eq!(paste(&display, "UTF8_STRING"), None);
    eprintln!("andris gave X display {display}'s clipboard up to another program");
}

/// Whether `owner`, which owns a selection, is asked for it within
/// `bound`.
fn asked_within(owner: &RustConnection, bound: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < bound {
        while let Some(event) = owner.poll_for_event().unwrap() {
            if let Event::SelectionRequest(_) = event {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[tokio::test]
async fn with_the_clipboard_off_andris_never_reads_it() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    // An owner that answers nothing, but notices being asked.
    let (owner, window, clipboard) = client(&display, "CLIPBOARD");
    owner
        .set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
        .unwrap();
    owner.flush().unwrap();

    let off = X11Platform::connect_with(
        &display,
        X11Options {
            clipboard: false,
            ..X11Options::default()
        },
    )
    .unwrap();
    assert!(off.platform().clipboard.is_none());
    assert!(off.set(Arc::from("not shared")).is_err());
    assert!(
        !asked_within(&owner, Duration::from_secs(1)),
        "andris asked for the clipboard with the clipboard off"
    );

    // The same owner is asked as soon as a platform with the clipboard on
    // starts, so the silence above was andris's, not the test's.
    let on = X11Platform::connect(&display).unwrap();
    assert!(on.platform().clipboard.is_some());
    assert!(
        asked_within(&owner, CLIPBOARD_BOUND),
        "andris did not ask for the clipboard with the clipboard on"
    );
    eprintln!("with the clipboard off, andris left X display {display}'s alone");
}

#[tokio::test]
async fn an_owner_that_never_answers_times_out_and_stalls_nothing() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let platform = X11Platform::connect(&display).unwrap();
    let mut changes = platform.changes();
    let copier = Copier::new(&display, None, b"before");
    assert_eq!(reported(&mut changes).await.as_deref(), Some("before"));

    // While andris waits for the squatter's text, it still takes text to
    // set, and serves it.
    let first = squat(&display);
    drop(copier);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let start = Instant::now();
    platform.set(Arc::from("meanwhile")).unwrap();
    pasted_eventually(&display, "meanwhile");
    assert!(start.elapsed() < CLIPBOARD_BOUND);
    assert!(
        changes.has_changed().unwrap(),
        "andris taking the clipboard withdraws the earlier text"
    );
    assert_eq!(*changes.borrow_and_update(), None);

    // Once andris has given up on a squatter's text, the earlier text is
    // gone: nothing is offered.
    let copier = Copier::new(&display, None, b"before again");
    assert_eq!(
        reported(&mut changes).await.as_deref(),
        Some("before again")
    );
    let second = squat(&display);
    drop(copier);
    let asked = Instant::now();
    tokio::time::timeout(CONVERSION_TIMEOUT + CLIPBOARD_BOUND, changes.changed())
        .await
        .expect("the conversion did not time out")
        .unwrap();
    let waited = asked.elapsed();
    assert!(
        waited >= CONVERSION_TIMEOUT - Duration::from_millis(500),
        "gave up after {waited:?}"
    );
    assert_eq!(*changes.borrow_and_update(), None);
    drop((first, second));
    eprintln!("andris gave up on an owner that never answered after {waited:?}");
}

// The real input and cursor paths. Until these, `tests/x11.rs` drove only
// capture, damage, resize and the clipboard: XTEST injection and the
// XFixes cursor source had no test anywhere, though Xvfb serves both. The
// server-side tests all use the synthetic backend, so nothing exercised
// the code that turns a client's bytes into events on the real desktop.

/// The backend, a connection of the test's own and the root window: what
/// each of the input and cursor tests needs before it starts.
fn rig(display: &str) -> (Arc<X11Platform>, RustConnection, Window) {
    let platform = X11Platform::connect(display).unwrap();
    let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
    let root = conn.setup().roots[screen_num].root;
    (platform, conn, root)
}

/// Releases a key or button the test pressed, however the test ends, so
/// that a failure does not leave the display holding it down and spoil
/// every test that runs after.
struct Held<'a> {
    platform: &'a Arc<X11Platform>,
    event: InputEvent,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let release = match self.event {
            InputEvent::Key { code, .. } => InputEvent::Key {
                code,
                pressed: false,
            },
            InputEvent::Button { button, .. } => InputEvent::Button {
                button,
                pressed: false,
            },
            other => other,
        };
        let _ = self.platform.inject_release(release);
    }
}

/// The keyboard state as the X server holds it: 256 bits, one a keycode.
fn pressed_keys(conn: &RustConnection) -> Vec<u8> {
    let keys = conn.query_keymap().unwrap().reply().unwrap().keys;
    let mut down = Vec::new();
    for (byte, bits) in keys.iter().enumerate() {
        for bit in 0..8 {
            if bits & (1 << bit) != 0 {
                down.push((byte * 8 + bit) as u8);
            }
        }
    }
    down
}

/// Wait until `read` returns a value `done` accepts, and return it.
fn until<T>(what: &str, read: impl Fn() -> T, done: impl Fn(&T) -> bool) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = read();
        if done(&value) {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn xtest_presses_and_releases_a_key_on_the_server() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let (platform, conn, _root) = rig(&display);

    // Nothing of ours is held to begin with; another client's key would
    // make this test meaningless, so it is asserted rather than assumed.
    let before = pressed_keys(&conn);
    assert!(
        before.is_empty(),
        "another client is holding keys down: {before:?}"
    );

    // KEY_A. The evdev code becomes an XKB key name and then a keycode,
    // so which keycode it lands on is the layout's business: what matters
    // is that exactly one key goes down, and that it is released again.
    let a = EvdevCode(30);
    let press = InputEvent::Key {
        code: a,
        pressed: true,
    };
    platform.inject(press).unwrap();
    // So a failure below does not leave the key held for later tests.
    let held = Held {
        platform: &platform,
        event: press,
    };
    let down = until(
        "the key to go down",
        || pressed_keys(&conn),
        |down| !down.is_empty(),
    );
    assert_eq!(down.len(), 1, "exactly one key is down: {down:?}");

    platform
        .inject(InputEvent::Key {
            code: a,
            pressed: false,
        })
        .unwrap();
    let up = until(
        "the key to come up",
        || pressed_keys(&conn),
        |down| down.is_empty(),
    );
    assert!(up.is_empty(), "the key was released: {up:?}");
    drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn xtest_moves_the_pointer_absolutely_and_relatively() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let (platform, conn, root) = rig(&display);
    let position = || {
        let at = conn.query_pointer(root).unwrap().reply().unwrap();
        (at.root_x, at.root_y)
    };

    platform
        .inject(InputEvent::PointerAbsolute { x: 120, y: 80 })
        .unwrap();
    assert_eq!(
        until("the pointer to arrive", position, |at| *at == (120, 80)),
        (120, 80)
    );

    // A relative move is applied from where the pointer is, which is the
    // path a client in server mouse mode drives.
    platform
        .inject(InputEvent::PointerRelative { dx: 10, dy: -20 })
        .unwrap();
    assert_eq!(
        until("the relative move", position, |at| *at == (130, 60)),
        (130, 60)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn xtest_presses_a_button_the_server_reports() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let (platform, conn, root) = rig(&display);
    let buttons = || u16::from(conn.query_pointer(root).unwrap().reply().unwrap().mask);
    const BUTTON_1: u16 = 0x0100;

    assert_eq!(buttons() & BUTTON_1, 0, "button 1 starts up");
    let press = InputEvent::Button {
        button: Button::Left,
        pressed: true,
    };
    platform.inject(press).unwrap();
    // As above: a failure must not leave the button down.
    let held = Held {
        platform: &platform,
        event: press,
    };
    until("button 1 to go down", buttons, |mask| mask & BUTTON_1 != 0);
    platform
        .inject(InputEvent::Button {
            button: Button::Left,
            pressed: false,
        })
        .unwrap();
    until("button 1 to come up", buttons, |mask| mask & BUTTON_1 == 0);
    drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn xkb_reports_the_lock_state_andris_sets() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let platform = X11Platform::connect(&display).unwrap();

    // set_lock_state presses and releases each lock key that differs,
    // through XTEST, and lock_state reads XKB's indicators back.
    let was = platform.lock_state().unwrap();
    let wanted = LockState {
        caps: !was.caps,
        num: was.num,
        scroll: was.scroll,
    };
    platform.set_lock_state(wanted).unwrap();
    let now = until(
        "caps lock to change",
        || platform.lock_state().unwrap(),
        |now| now.caps == wanted.caps,
    );
    assert_eq!(now.caps, wanted.caps);

    // Put it back, so the display is left as it was found.
    platform.set_lock_state(was).unwrap();
    until(
        "caps lock to go back",
        || platform.lock_state().unwrap(),
        |now| now.caps == was.caps,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn xfixes_reports_the_root_cursor_and_its_changes() {
    let Some(display) = display() else {
        return;
    };
    let _turn = DISPLAY_TURN.lock().await;
    let (platform, conn, root) = rig(&display);
    let mut cursors = platform.subscribe();

    // What XFixes reported when the thread started, so that what this
    // waits for below is a change rather than the initial read.
    let first = cursors.borrow_and_update().clone();

    // A 16x16 core cursor with a hot spot nothing else would have, built
    // from two depth-1 pixmaps, so no Render extension is needed.
    const SIDE: u16 = 16;
    const XHOT: u16 = 3;
    const YHOT: u16 = 5;
    let source = conn.generate_id().unwrap();
    let mask = conn.generate_id().unwrap();
    conn.create_pixmap(1, source, root, SIDE, SIDE).unwrap();
    conn.create_pixmap(1, mask, root, SIDE, SIDE).unwrap();
    // Fill the mask so every pixel of the cursor is opaque, and leave the
    // source clear, which makes a solid block of the background colour.
    let gc = conn.generate_id().unwrap();
    conn.create_gc(gc, mask, &CreateGCAux::new().foreground(1))
        .unwrap();
    conn.poly_fill_rectangle(
        mask,
        gc,
        &[xproto::Rectangle {
            x: 0,
            y: 0,
            width: SIDE,
            height: SIDE,
        }],
    )
    .unwrap();
    let cursor = conn.generate_id().unwrap();
    conn.create_cursor(
        cursor, source, mask, 0xFFFF, 0xFFFF, 0xFFFF, 0, 0, 0, XHOT, YHOT,
    )
    .unwrap();
    conn.change_window_attributes(
        root,
        &xproto::ChangeWindowAttributesAux::new().cursor(cursor),
    )
    .unwrap();
    conn.flush().unwrap();

    // The cursor thread reports it through the watch channel. The hot
    // spot is part of what is waited for, not just asserted afterwards:
    // Xvfb's own root cursor is 16x16 too, with its hot spot at (7, 7),
    // so a wait on the size alone finishes before ours has arrived.
    let deadline = Instant::now() + Duration::from_secs(5);
    let shape = loop {
        if let Some(shape) = cursors.borrow_and_update().clone()
            && shape.width() == SIDE
            && shape.height() == SIDE
            && shape.hot_x() == XHOT
            && shape.hot_y() == YHOT
        {
            break shape;
        }
        assert!(
            Instant::now() < deadline,
            "XFixes never reported the {SIDE}x{SIDE} cursor with its hot spot at ({XHOT}, {YHOT})"
        );
        tokio::time::timeout(Duration::from_millis(200), cursors.changed())
            .await
            .ok();
    };
    assert_ne!(
        Some(&shape),
        first.as_ref(),
        "the shape waited for is a change, not the one already reported"
    );
    assert_eq!(
        shape.argb().len(),
        SIDE as usize * SIDE as usize,
        "one ARGB word a pixel"
    );
    assert!(
        shape.argb().iter().all(|pixel| pixel >> 24 == 0xFF),
        "the mask was filled, so every pixel is opaque"
    );

    // Leave the root window without our cursor, and free it.
    conn.change_window_attributes(
        root,
        &xproto::ChangeWindowAttributesAux::new().cursor(x11rb::NONE),
    )
    .unwrap();
    conn.free_cursor(cursor).unwrap();
    conn.free_gc(gc).unwrap();
    conn.free_pixmap(source).unwrap();
    conn.free_pixmap(mask).unwrap();
    conn.flush().unwrap();
}
