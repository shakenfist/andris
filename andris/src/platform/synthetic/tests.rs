//! The synthetic platform's tests.
//!
//! In a file of their own, as the qemu platform's are: they are about as
//! long as the backend itself, and `mod.rs` is easier to read as one
//! type without them. `use super::*` reaches the backend exactly as it
//! did when this was an inline module, private items included.

use std::time::Duration;

use super::*;
use crate::platform::Region;

const SIZE: Size = Size {
    width: 1024,
    height: 768,
};

fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
    let row = frame.row(y).unwrap();
    let at = x as usize * 4;
    row[at..at + 4].try_into().unwrap()
}

#[test]
fn capture_has_the_size_and_a_padded_stride() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let frame = platform.capture_all().unwrap().into_whole().unwrap();
    assert_eq!(frame.size(), SIZE);
    assert_eq!(platform.size().unwrap(), SIZE);
    assert_eq!(frame.stride(), 1024 * 4 + STRIDE_PADDING);
    let row = frame.stride();
    assert!(
        frame.data()[row - STRIDE_PADDING..row]
            .iter()
            .all(|&b| b == PADDING_BYTE)
    );
}

#[test]
fn capture_is_deterministic() {
    let a = SyntheticPlatform::new(SIZE).unwrap();
    let b = SyntheticPlatform::new(SIZE).unwrap();
    assert_eq!(a.capture_all().unwrap(), b.capture_all().unwrap());
    assert_eq!(a.desktop().unwrap(), b.desktop().unwrap());
}

#[test]
fn pattern_has_bars_and_the_square() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let frame = platform.desktop().unwrap();
    // BGRX: white, then yellow in the second bar, black in the last.
    assert_eq!(pixel(&frame, 0, 0), [255, 255, 255, 0]);
    assert_eq!(pixel(&frame, 128, 0), [0, 255, 255, 0]);
    assert_eq!(pixel(&frame, 1023, 767), [0, 0, 0, 0]);
    // The square, orange, at (0, SQUARE_TOP).
    assert_eq!(pixel(&frame, 0, SQUARE_TOP), [0, 128, 255, 0]);
    assert_eq!(pixel(&frame, 63, SQUARE_TOP + 63), [0, 128, 255, 0]);
    assert_eq!(pixel(&frame, 64, SQUARE_TOP), [255, 255, 255, 0]);
    assert_eq!(pixel(&frame, 0, SQUARE_TOP - 1), [255, 255, 255, 0]);
}

#[test]
fn advance_changes_only_bands_one_and_two() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let before = platform.desktop().unwrap();
    platform.advance().unwrap();
    assert_eq!(platform.square_x().unwrap(), SQUARE_STEP);
    let after = platform.desktop().unwrap();
    let changed: Vec<u32> = (0..SIZE.height.div_ceil(64))
        .filter(|band| {
            let top = band * 64;
            let rows = 64.min(SIZE.height - top);
            before.band(top, rows) != after.band(top, rows)
        })
        .collect();
    assert_eq!(changed, vec![1, 2]);
}

#[test]
fn advance_wraps_at_the_right_edge() {
    let platform = SyntheticPlatform::new(Size {
        width: 80,
        height: 200,
    })
    .unwrap();
    platform.advance().unwrap();
    platform.advance().unwrap();
    assert_eq!(platform.square_x().unwrap(), 16);
    platform.advance().unwrap();
    assert_eq!(platform.square_x().unwrap(), 0);
}

#[test]
fn small_desktop_has_no_square() {
    let platform = SyntheticPlatform::new(Size {
        width: 32,
        height: 32,
    })
    .unwrap();
    let frame = platform.desktop().unwrap();
    assert_eq!(pixel(&frame, 0, 0), [255, 255, 255, 0]);
    platform.advance().unwrap();
    assert_eq!(platform.square_x().unwrap(), 0);
}

#[test]
fn set_size_changes_geometry_and_capture() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    for _ in 0..10 {
        platform.advance().unwrap();
    }
    let smaller = Size {
        width: 100,
        height: 200,
    };
    platform.set_size(smaller).unwrap();
    assert_eq!(platform.size().unwrap(), smaller);
    assert_eq!(platform.desktop().unwrap().size(), smaller);
    assert_eq!(platform.square_x().unwrap(), 0, "the square moved back");
    assert!(
        platform
            .set_size(Size {
                width: 0,
                height: 1
            })
            .is_err()
    );
}

#[test]
fn fail_captures_fails_that_many_of_either_kind() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    platform.fail_captures(3).unwrap();
    let rects = [Rectangle::new(0, 0, 8, 8)];
    let errors = [
        platform.capture_all().unwrap_err(),
        platform.capture(&rects).unwrap_err(),
        platform.capture_all().unwrap_err(),
    ];
    for error in errors {
        assert!(!crate::platform::is_gone(&error));
    }
    assert!(platform.desktop().is_ok(), "desktop() is not a capture");
    assert_eq!(platform.capture(&rects).unwrap().patches().len(), 1);
    assert_eq!(platform.capture_all().unwrap().size(), SIZE);
}

/// The pending damage, as the tiles' merged rectangles.
fn damaged(subscription: &DamageSubscription) -> Vec<Rectangle> {
    subscription.take().unwrap().rectangles(usize::MAX)
}

#[test]
fn advance_damages_the_old_and_new_squares() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let subscription = platform.subscribe_damage().unwrap();
    assert_eq!(damaged(&subscription), vec![Rectangle::full(SIZE)]);
    assert!(damaged(&subscription).is_empty(), "nothing yet");

    // 0..64 and 8..72 across, rows 96..160: tile columns 0 and 1,
    // tile rows 1 and 2.
    platform.advance().unwrap();
    let mut expected = Region::empty(SIZE);
    expected.add(Rectangle::new(0, SQUARE_TOP, SQUARE_SIZE, SQUARE_SIZE));
    expected.add(Rectangle::new(
        SQUARE_STEP,
        SQUARE_TOP,
        SQUARE_SIZE,
        SQUARE_SIZE,
    ));
    let region = subscription.take().unwrap();
    assert_eq!(region, expected);
    assert_eq!(region.rectangles(32), vec![Rectangle::new(0, 64, 128, 128)]);

    // The wrap damages both ends.
    let narrow = SyntheticPlatform::new(Size {
        width: 200,
        height: 200,
    })
    .unwrap();
    let subscription = narrow.subscribe_damage().unwrap();
    for _ in 0..17 {
        narrow.advance().unwrap();
    }
    subscription.take().unwrap();
    assert_eq!(narrow.square_x().unwrap(), 136);
    narrow.advance().unwrap();
    assert_eq!(narrow.square_x().unwrap(), 0);
    assert_eq!(
        damaged(&subscription),
        vec![
            Rectangle::new(0, 64, 64, 128),
            Rectangle::new(128, 64, 72, 128)
        ]
    );
}

#[test]
fn advance_damages_nothing_without_a_square() {
    let platform = SyntheticPlatform::new(Size {
        width: 32,
        height: 32,
    })
    .unwrap();
    let subscription = platform.subscribe_damage().unwrap();
    subscription.take().unwrap();
    platform.advance().unwrap();
    assert!(damaged(&subscription).is_empty());
}

#[test]
fn set_size_damages_the_whole_new_desktop() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let subscription = platform.subscribe_damage().unwrap();
    subscription.take().unwrap();
    let smaller = Size {
        width: 100,
        height: 200,
    };
    platform.set_size(smaller).unwrap();
    assert_eq!(subscription.take().unwrap(), Region::full(smaller));
}

#[test]
fn set_size_unreported_changes_capture_and_reports_nothing() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let subscription = platform.subscribe_damage().unwrap();
    subscription.take().unwrap();
    let smaller = Size {
        width: 100,
        height: 200,
    };
    platform.set_size_unreported(smaller).unwrap();
    assert_eq!(platform.capture_all().unwrap().size(), smaller);
    assert!(damaged(&subscription).is_empty());
}

#[test]
fn paint_undamaged_changes_pixels_and_reports_nothing() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let subscription = platform.subscribe_damage().unwrap();
    subscription.take().unwrap();
    let before = platform.desktop().unwrap();
    platform
        .paint_undamaged(Rectangle::new(500, 500, 10, 10), (1, 2, 3))
        .unwrap();
    // Over the square too, and a later paint on top of an earlier.
    platform
        .paint_undamaged(Rectangle::new(0, SQUARE_TOP, 4, 4), (9, 9, 9))
        .unwrap();
    platform
        .paint_undamaged(Rectangle::new(2, SQUARE_TOP, 4, 4), (7, 7, 7))
        .unwrap();
    let after = platform.desktop().unwrap();
    assert_ne!(before, after);
    assert_eq!(pixel(&after, 500, 500), [3, 2, 1, 0]);
    assert_eq!(pixel(&after, 509, 509), [3, 2, 1, 0]);
    assert_eq!(pixel(&after, 510, 509), pixel(&before, 510, 509));
    assert_eq!(pixel(&after, 1, SQUARE_TOP), [9, 9, 9, 0]);
    assert_eq!(pixel(&after, 2, SQUARE_TOP), [7, 7, 7, 0]);
    assert_eq!(pixel(&after, 6, SQUARE_TOP + 4), [0, 128, 255, 0]);
    assert!(damaged(&subscription).is_empty());
}

#[test]
fn paints_report_their_damage() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let subscription = platform.subscribe_damage().unwrap();
    subscription.take().unwrap();
    platform
        .paint(Rectangle::new(10, 10, 4, 4), (1, 2, 3))
        .unwrap();
    assert_eq!(damaged(&subscription), vec![Rectangle::new(0, 0, 64, 64)]);
    assert_eq!(pixel(&platform.desktop().unwrap(), 12, 12), [3, 2, 1, 0]);

    let photo = Rectangle::new(128, 0, 128, 64);
    platform.paint_photo(photo).unwrap();
    assert_eq!(damaged(&subscription), vec![photo]);
    let desktop = platform.desktop().unwrap();
    // Not flat: neighbours differ.
    assert_ne!(pixel(&desktop, 130, 5), pixel(&desktop, 131, 5));
    // Painting it again changes no pixel.
    platform.paint_photo(photo).unwrap();
    assert_eq!(platform.desktop().unwrap(), desktop);
}

#[test]
fn animate_pans_a_photograph_and_moves_without_adding_overlays() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let subscription = platform.subscribe_damage().unwrap();
    subscription.take().unwrap();
    let pattern = platform.desktop().unwrap();
    let first = Rectangle::new(128, 64, 128, 128);
    platform.animate(first).unwrap();
    assert_eq!(damaged(&subscription), vec![first]);
    let one = platform.desktop().unwrap();
    assert_ne!(pixel(&one, 130, 70), pixel(&one, 131, 70), "flat");

    // The next frame changes nearly every pixel of it, and nothing
    // else.
    platform.animate(first).unwrap();
    assert_eq!(damaged(&subscription), vec![first]);
    let two = platform.desktop().unwrap();
    let mut differ = 0;
    for y in 64..192 {
        for x in 128..256 {
            differ += usize::from(pixel(&one, x, y) != pixel(&two, x, y));
        }
    }
    assert!(differ > 128 * 128 * 9 / 10, "only {differ} pixels changed");
    assert_eq!(pixel(&two, 10, 10), pixel(&pattern, 10, 10));

    // Moving it reports both rectangles, and shows the pattern where
    // it was.
    let second = Rectangle::new(512, 384, 128, 128);
    platform.animate(second).unwrap();
    assert_eq!(damaged(&subscription), vec![first, second]);
    let moved = platform.desktop().unwrap();
    assert_eq!(pixel(&moved, 130, 70), pixel(&pattern, 130, 70));
    assert_ne!(pixel(&moved, 520, 390), pixel(&pattern, 520, 390));
    assert!(platform.state().unwrap().overlays.is_empty());
}

#[test]
fn report_damage_changes_nothing() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let subscription = platform.subscribe_damage().unwrap();
    subscription.take().unwrap();
    let before = platform.desktop().unwrap();
    platform
        .report_damage(Rectangle::new(300, 300, 1, 1))
        .unwrap();
    assert_eq!(platform.desktop().unwrap(), before);
    assert_eq!(
        damaged(&subscription),
        vec![Rectangle::new(256, 256, 64, 64)]
    );
}

#[test]
fn captured_patches_equal_the_desktop() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    platform.advance().unwrap();
    platform
        .paint_undamaged(Rectangle::new(60, 100, 30, 30), (10, 20, 30))
        .unwrap();
    let desktop = platform.desktop().unwrap();
    let rects = [
        Rectangle::new(0, 64, 128, 128),
        Rectangle::new(1000, 700, 100, 100),
        Rectangle::new(5, 7, 1, 1),
        Rectangle::new(2000, 0, 10, 10),
    ];
    let capture = platform.capture(&rects).unwrap();
    assert_eq!(capture.size(), SIZE);
    let got: Vec<Rectangle> = capture.patches().iter().map(Patch::rect).collect();
    assert_eq!(
        got,
        vec![
            Rectangle::new(0, 64, 128, 128),
            Rectangle::new(1000, 700, 24, 68),
            Rectangle::new(5, 7, 1, 1),
        ],
        "clipped, and the rectangle outside dropped"
    );
    for patch in capture.patches() {
        let pixels = patch.pixels();
        assert_eq!(
            pixels.stride(),
            pixels.width() as usize * 4 + STRIDE_PADDING
        );
        let expected = desktop.crop(patch.rect()).unwrap();
        for y in 0..pixels.height() {
            assert_eq!(pixels.row(y), expected.row(y), "{} row {y}", patch.rect());
        }
    }
    assert_eq!(
        platform.capture_all().unwrap().into_whole().unwrap(),
        desktop
    );
}

#[test]
fn fail_size_reads_fails_that_many() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    platform.fail_size_reads(2).unwrap();
    for _ in 0..2 {
        let error = platform.size().unwrap_err();
        assert!(!crate::platform::is_gone(&error));
    }
    assert_eq!(platform.size().unwrap(), SIZE);
}

#[test]
fn records_input_and_toggles_locks() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let seams = platform.platform();
    let events = [
        InputEvent::Key {
            code: EvdevCode(30),
            pressed: true,
        },
        InputEvent::PointerAbsolute { x: 5, y: 6 },
        InputEvent::Key {
            code: EvdevCode::CAPSLOCK,
            pressed: true,
        },
        InputEvent::Key {
            code: EvdevCode::CAPSLOCK,
            pressed: false,
        },
    ];
    for event in events {
        seams.input.inject(event).unwrap();
    }
    assert_eq!(platform.input_events().unwrap(), events);
    assert_eq!(
        seams.input.lock_state().unwrap(),
        LockState {
            caps: true,
            num: false,
            scroll: false
        }
    );
    assert_eq!(platform.take_input_events().unwrap(), events);
    assert!(platform.input_events().unwrap().is_empty());
}

#[test]
fn only_the_most_recent_events_are_kept() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let extra = 10;
    for x in 0..(RECORDED_EVENTS + extra) as u32 {
        platform
            .inject(InputEvent::PointerAbsolute { x, y: 0 })
            .unwrap();
    }
    let events = platform.take_input_events().unwrap();
    assert_eq!(events.len(), RECORDED_EVENTS);
    assert_eq!(
        events.first(),
        Some(&InputEvent::PointerAbsolute {
            x: extra as u32,
            y: 0
        })
    );
    assert_eq!(
        events.last(),
        Some(&InputEvent::PointerAbsolute {
            x: (RECORDED_EVENTS + extra - 1) as u32,
            y: 0
        })
    );
}

#[test]
fn set_lock_state_converges() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    platform
        .force_lock_state(LockState {
            caps: true,
            num: false,
            scroll: true,
        })
        .unwrap();
    let wanted = LockState {
        caps: false,
        num: true,
        scroll: true,
    };
    platform.set_lock_state(wanted).unwrap();
    assert_eq!(platform.lock_state().unwrap(), wanted);
    assert_eq!(platform.take_input_events().unwrap().len(), 4);
    platform.set_lock_state(wanted).unwrap();
    assert!(platform.input_events().unwrap().is_empty());
}

#[test]
fn cursor_starts_with_the_arrow_and_follows_changes() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let mut cursor = platform.platform().cursor.subscribe();
    let first = cursor.borrow_and_update().clone().unwrap();
    assert_eq!(*first, default_cursor());
    assert_eq!((first.width(), first.height()), (32, 32));
    assert_eq!((first.hot_x(), first.hot_y()), (0, 0));
    assert!(!cursor.has_changed().unwrap());

    let dot = CursorShape::new(1, 1, 0, 0, vec![0xFF00_0000]).unwrap();
    platform.set_cursor(Some(dot.clone()));
    assert!(cursor.has_changed().unwrap());
    assert_eq!(*cursor.borrow_and_update().clone().unwrap(), dot);
    platform.set_cursor(None);
    assert!(cursor.borrow_and_update().is_none());
}

#[test]
fn display_control_resizes_unless_refusing() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let control = platform.platform().display_control;
    let subscription = platform.subscribe_damage().unwrap();
    subscription.take().unwrap();
    let smaller = Size {
        width: 640,
        height: 480,
    };
    assert_eq!(control.set_size(smaller).unwrap(), Resize::Applied);
    assert_eq!(platform.size().unwrap(), smaller);
    assert_eq!(subscription.take().unwrap(), Region::full(smaller));
    assert_eq!(control.set_size(smaller).unwrap(), Resize::Unchanged);

    let zero = Size {
        width: 0,
        height: 480,
    };
    assert!(matches!(
        control.set_size(zero).unwrap(),
        Resize::Refused(_)
    ));

    platform.refuse_resizes(true).unwrap();
    assert!(matches!(
        control.set_size(SIZE).unwrap(),
        Resize::Refused(_)
    ));
    assert_eq!(
        platform.size().unwrap(),
        smaller,
        "a refusal changes nothing"
    );
    assert!(damaged(&subscription).is_empty());
    platform.refuse_resizes(false).unwrap();
    assert_eq!(control.set_size(SIZE).unwrap(), Resize::Applied);
}

#[test]
fn the_clipboard_reports_copies_and_keeps_what_is_set() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let clipboard = platform
        .platform()
        .clipboard
        .expect("the synthetic platform shares a clipboard");
    let mut changes = clipboard.changes();
    assert!(changes.borrow_and_update().is_none());

    platform.copy_text(Some("copied"));
    assert!(changes.has_changed().unwrap());
    assert_eq!(changes.borrow_and_update().as_deref(), Some("copied"));
    platform.copy_text(Some("copied"));
    assert!(
        changes.has_changed().unwrap(),
        "the same text again is a change"
    );
    changes.mark_unchanged();

    clipboard.set(Arc::from("pasted")).unwrap();
    assert!(changes.has_changed().unwrap());
    assert!(
        changes.borrow_and_update().is_none(),
        "the copied text is gone, and andris's own is not reported"
    );
    assert_eq!(platform.clipboard_set().unwrap().as_deref(), Some("pasted"));
    assert_eq!(platform.clipboard_sets().unwrap(), 1);
    clipboard.set(Arc::from("pasted again")).unwrap();
    assert!(!changes.has_changed().unwrap(), "nothing more has gone");

    platform.copy_text(None);
    assert!(changes.borrow_and_update().is_none());
}

#[test]
fn default_cursor_passes_the_shape_checks() {
    let cursor = default_cursor();
    let checked = CursorShape::new(
        cursor.width(),
        cursor.height(),
        cursor.hot_x(),
        cursor.hot_y(),
        cursor.argb().to_vec(),
    )
    .unwrap();
    assert_eq!(checked, cursor);
}

#[test]
fn default_cursor_is_opaque_at_the_tip_and_clear_in_the_corner() {
    let cursor = default_cursor();
    assert_eq!(cursor.argb()[0], 0xFF00_0000);
    assert_eq!(cursor.argb()[31], 0);
    assert!(cursor.argb().contains(&0xFFFF_FFFF));
}

#[tokio::test]
async fn the_platform_sounds_the_default_tone() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    assert!(
        platform.platform().audio.is_none(),
        "the plain platform has no sound: run decides it"
    );
    let audio = platform.platform_with_tone().audio.unwrap();
    let mut receiver = audio.open().unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(frame.peak() > 8000, "the pattern starts with the sound");
}

// The four contracts below are the ones the closing push audit found
// the synthetic backend had drifted away from: each is what a real
// backend already does, and none of them could be reached through
// this one before.

/// [`DisplayControl::set_size`], which the inherent `set_size`
/// would otherwise shadow.
fn resize(platform: &SyntheticPlatform, width: u32, height: u32) -> Resize {
    DisplayControl::set_size(platform, Size { width, height }).unwrap()
}

/// Every seam that can report [`Gone`] does, once the desktop has
/// vanished. `CursorSource::subscribe` and `Clipboard::changes` are not
/// among them: they hand back a receiver rather than a `Result`, so they
/// have nothing to fail with, and a consumer learns the desktop went
/// when the sender drops.
#[test]
fn every_fallible_seam_is_gone_once_the_desktop_has_vanished() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let whole = Rectangle::full(SIZE);
    // Each of these succeeds while the desktop is here.
    platform.size().unwrap();
    platform.capture(&[whole]).unwrap();
    platform.capture_all().unwrap();
    platform
        .inject(InputEvent::Key {
            code: EvdevCode(30),
            pressed: true,
        })
        .unwrap();
    platform.lock_state().unwrap();
    resize(&platform, 800, 600);
    Clipboard::set(&*platform, Arc::from("here")).unwrap();

    platform.vanish("the X server exited").unwrap();

    // And every one of them is Gone afterwards, as X11 reports
    // through stopped(): a capture or a resize mid-session now
    // reaches the server's gone handling.
    let gone = |error: anyhow::Error| {
        assert!(crate::platform::is_gone(&error), "{error:#}");
        assert!(
            error.to_string().contains("the X server exited"),
            "{error:#}"
        );
    };
    gone(platform.size().unwrap_err());
    gone(platform.capture(&[whole]).unwrap_err());
    gone(platform.capture_all().unwrap_err());
    gone(platform.lock_state().unwrap_err());
    gone(
        platform
            .inject(InputEvent::Key {
                code: EvdevCode(30),
                pressed: false,
            })
            .unwrap_err(),
    );
    gone(
        DisplayControl::set_size(
            &*platform,
            Size {
                width: 640,
                height: 480,
            },
        )
        .unwrap_err(),
    );
    // inject_release shares queue() with inject, but a release andris
    // owes the desktop takes its own way in, so check that too.
    gone(
        platform
            .inject_release(InputEvent::Key {
                code: EvdevCode(30),
                pressed: false,
            })
            .unwrap_err(),
    );
    gone(Clipboard::set(&*platform, Arc::from("gone")).unwrap_err());
    gone(platform.subscribe_damage().unwrap_err());
}

#[test]
fn a_held_queue_fills_up_and_then_refuses_events() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let motion = InputEvent::PointerRelative { dx: 1, dy: 0 };
    // Nothing waits while the desktop is applying events as they
    // arrive, however many are injected.
    for _ in 0..INPUT_QUEUE_LIMIT + 1 {
        platform.inject(motion).unwrap();
    }
    assert_eq!(platform.queued_input(), 0);

    // Held, the queue fills and then refuses, as a real backend's
    // does when its input thread cannot keep up.
    platform.hold_input(true).unwrap();
    for _ in 0..INPUT_QUEUE_LIMIT {
        platform.inject(motion).unwrap();
    }
    assert_eq!(platform.queued_input(), INPUT_QUEUE_LIMIT);
    let error = platform.inject(motion).unwrap_err().to_string();
    assert!(error.contains("input queue is full"), "{error}");
    // A refusal costs the queue nothing.
    assert_eq!(platform.queued_input(), INPUT_QUEUE_LIMIT);

    // Released, it takes events again.
    platform.hold_input(false).unwrap();
    assert_eq!(platform.queued_input(), 0);
    platform.inject(motion).unwrap();
}

#[test]
fn an_owed_release_survives_a_full_queue() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let press = InputEvent::Key {
        code: EvdevCode(30),
        pressed: true,
    };
    let release = InputEvent::Key {
        code: EvdevCode(30),
        pressed: false,
    };
    platform.hold_input(true).unwrap();
    for _ in 0..INPUT_QUEUE_LIMIT {
        platform.inject(press).unwrap();
    }
    // A client's own event is refused, but the release andris owes
    // the desktop draws on the reserve and gets through.
    assert!(platform.inject(release).is_err());
    for _ in 0..RELEASE_RESERVE {
        platform.inject_release(release).unwrap();
    }
    // The reserve is bounded in its turn.
    assert!(platform.inject_release(release).is_err());
    assert_eq!(platform.queued_input(), INPUT_QUEUE_LIMIT + RELEASE_RESERVE);
}

#[test]
fn clipboard_text_over_the_limit_is_dropped() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    Clipboard::set(&*platform, Arc::from("small")).unwrap();
    assert_eq!(
        platform.clipboard_set().unwrap().as_deref(),
        Some("small"),
        "text within the limit is kept"
    );
    // Exactly at the limit is still within it.
    let edge: Arc<str> = Arc::from("y".repeat(MAX_CLIPBOARD_TEXT));
    Clipboard::set(&*platform, edge).unwrap();
    assert_eq!(
        platform.clipboard_set().unwrap().map(|text| text.len()),
        Some(MAX_CLIPBOARD_TEXT),
        "text of exactly the limit is kept"
    );
    let counted = platform.clipboard_sets().unwrap();

    // One byte over: dropped with a warning, as X11 drops it, and what
    // was there is left alone and not counted as a set.
    let big: Arc<str> = Arc::from("x".repeat(MAX_CLIPBOARD_TEXT + 1));
    Clipboard::set(&*platform, big).unwrap();
    assert_eq!(
        platform.clipboard_set().unwrap().map(|text| text.len()),
        Some(MAX_CLIPBOARD_TEXT),
        "the oversize text was not kept"
    );
    assert_eq!(
        platform.clipboard_sets().unwrap(),
        counted,
        "a dropped set is not counted"
    );
}

#[test]
fn a_size_range_clamps_and_refuses_at_its_limit() {
    let platform = SyntheticPlatform::new(SIZE).unwrap();
    let range = SizeRange {
        min: Size {
            width: 320,
            height: 200,
        },
        max: Size {
            width: 1280,
            height: 1024,
        },
    };
    platform.set_size_range(Some(range)).unwrap();

    // Outside the range: clamped, and applied at the clamp, so the
    // client sees the new size as it sees any other.
    assert_eq!(resize(&platform, 4000, 4000), Resize::Applied);
    assert_eq!(
        platform.size().unwrap(),
        Size {
            width: 1280,
            height: 1024,
        }
    );

    // Asked for more again, with nothing left to give: refused
    // rather than Unchanged, because the client asked for a size it
    // did not get and nothing else would tell it so.
    let refused = resize(&platform, 4000, 4000);
    match refused {
        Resize::Refused(why) => {
            assert!(why.contains("the nearest it allows"), "{why}");
            assert!(why.contains("320x200 to 1280x1024"), "{why}");
        }
        other => panic!("expected a refusal at the limit, got {other:?}"),
    }

    // The minimum clamps too, and each side on its own.
    assert_eq!(resize(&platform, 10, 10), Resize::Applied);
    assert_eq!(
        platform.size().unwrap(),
        Size {
            width: 320,
            height: 200,
        }
    );
    assert_eq!(resize(&platform, 4000, 500), Resize::Applied);
    assert_eq!(
        platform.size().unwrap(),
        Size {
            width: 1280,
            height: 500,
        }
    );

    // The size it already has is Unchanged, as ever. (1280x500 now,
    // from the one-sided clamp just above.)
    assert_eq!(resize(&platform, 1280, 500), Resize::Unchanged);

    // And with no range, any size Size::check allows is applied.
    platform.set_size_range(None).unwrap();
    assert_eq!(resize(&platform, 4000, 4000), Resize::Applied);
}
