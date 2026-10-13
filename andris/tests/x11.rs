//! The X11 backend against a real X server: damage from a drawing, the
//! drawing captured through MIT-SHM and through `GetImage`, capture
//! carrying on after another client detaches andris's MIT-SHM segment, and
//! resizes through RandR.
//!
//! The X server is named by `ANDRIS_TEST_DISPLAY`. `make e2e` sets it to
//! its Xvfb, before any window is mapped; with it unset, as under
//! `make test`, the test says on stderr that it is skipping, and passes
//! without doing anything. The display must be one nothing else draws on
//! or attaches MIT-SHM segments to while the test runs, and needs the
//! X-Resource extension, 1.2 or later. The tests take turns with the
//! display, and leave a filled rectangle on its root window.
//!
//! The resize test needs an X server that resizes up to its starting size
//! and no further, as Xvfb does: its `-screen` size is the largest RandR
//! allows. It leaves the display at that size.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

use std::io::Write as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use andris::platform::x11::{CaptureMethod, X11Options, X11Platform};
use andris::platform::{
    Capture, DamageSubscription, DisplayControl, DisplayGeometry, Frame, Rectangle, Region, Resize,
    ScreenSource, Size, TILE, is_gone,
};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _, CreateGCAux, SubwindowMode, Window};
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
fn display() -> Option<String> {
    let display = std::env::var("ANDRIS_TEST_DISPLAY")
        .ok()
        .filter(|name| !name.is_empty());
    if let Some(name) = &display {
        KEEP_OPEN.get_or_init(|| x11rb::connect(Some(name)).unwrap().0);
    } else {
        // Straight to stderr: libtest captures eprintln!, which would
        // hide that this passed without testing anything.
        let _ = writeln!(
            std::io::stderr(),
            "ANDRIS_TEST_DISPLAY is unset; SKIPPING the X11 backend test, which `make e2e` runs"
        );
    }
    display
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

    let get_image = X11Platform::connect_with(&display, X11Options { shm: false }).unwrap();
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
