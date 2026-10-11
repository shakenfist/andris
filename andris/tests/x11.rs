//! The X11 backend against a real X server: damage from a drawing, and
//! the drawing captured through MIT-SHM and through `GetImage`.
//!
//! The X server is named by `ANDRIS_TEST_DISPLAY`. `make e2e` sets it to
//! its Xvfb, before any window is mapped; with it unset, as under
//! `make test`, the test passes without doing anything. The display must
//! be one nothing else draws on while the test runs, and the test leaves
//! a filled rectangle on its root window.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use andris::platform::x11::{CaptureMethod, X11Options, X11Platform};
use andris::platform::{
    Capture, DamageSubscription, DisplayGeometry, Frame, Rectangle, Region, ScreenSource, TILE,
};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, CreateGCAux, SubwindowMode};

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

/// The display to test against, or `None` to skip.
fn display() -> Option<String> {
    std::env::var("ANDRIS_TEST_DISPLAY")
        .ok()
        .filter(|name| !name.is_empty())
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

#[tokio::test]
async fn damage_and_both_capture_paths_see_a_drawing() {
    let Some(display) = display() else {
        eprintln!("ANDRIS_TEST_DISPLAY is unset; skipping the X11 test");
        return;
    };
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
    eprintln!("damage, MIT-SHM and GetImage agree on X display {display}");
}
