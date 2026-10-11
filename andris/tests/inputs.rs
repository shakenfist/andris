//! The inputs channel: its `INIT`, keys, lock keys, the pointer, buttons,
//! motion acks, and releasing what is held when the channel closes,
//! against an in-process andris on the synthetic platform.
//!
//! The synthetic platform records every event injected. A test that
//! needs to know the server has read everything it sent ends with a
//! sentinel key, F12 pressed and released, and waits for that.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use andris::platform::{Button, EvdevCode, InputEvent, LockState, Size};
use andris::server::inputs::GEOMETRY_MAX_AGE;
use andris::server::session::MouseMode;
use shakenfist_spice_protocol::constants::{keyboard_modifiers, mouse_button_id};
use shakenfist_spice_protocol::messages::{
    InputsInit, KeyEvent, KeyModifiers, KeyScancode, MouseButton, MouseMotion, MousePosition,
    WireType,
};
use shakenfist_spice_protocol::{ChannelType, inputs_client, inputs_server};

use support::{TestClient, TestServer, WAIT};

/// How long to wait before concluding the server sends nothing.
const QUIET: Duration = Duration::from_millis(100);

/// The sentinel key, F12.
const SENTINEL: u32 = 0x58;

fn body<T: WireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write(&mut out);
    out
}

fn key(code: u16, pressed: bool) -> InputEvent {
    InputEvent::Key {
        code: EvdevCode(code),
        pressed,
    }
}

fn button(button: Button, pressed: bool) -> InputEvent {
    InputEvent::Button { button, pressed }
}

/// A server, the main channel that holds its session open, an inputs
/// channel, and the modifiers its `INIT` carried.
async fn open_inputs(server: &TestServer) -> (TestClient, TestClient, u16) {
    let (main, session) = server.open_main().await;
    let mut inputs = server.open(session, ChannelType::Inputs).await;
    let message = inputs.recv().await;
    assert_eq!(message.header.message_type, inputs_server::INIT);
    let init = InputsInit::decode(message.payload()).unwrap();
    (main, inputs, init.keyboard_modifiers)
}

async fn send_key(client: &mut TestClient, opcode: u16, scancode: u32) {
    client.send(opcode, &body(&KeyEvent { scancode })).await;
}

async fn send_position(client: &mut TestClient, x: u32, y: u32, display_id: u8) {
    let position = MousePosition {
        x,
        y,
        buttons_state: 0,
        display_id,
    };
    client
        .send(inputs_client::MOUSE_POSITION, &body(&position))
        .await;
}

async fn send_motion(client: &mut TestClient, dx: i32, dy: i32) {
    let motion = MouseMotion {
        dx,
        dy,
        buttons_state: 0,
    };
    client
        .send(inputs_client::MOUSE_MOTION, &body(&motion))
        .await;
}

async fn send_button(client: &mut TestClient, opcode: u16, id: u8) {
    let message = MouseButton {
        button: id,
        buttons_state: 0,
    };
    client.send(opcode, &body(&message)).await;
}

async fn send_modifiers(client: &mut TestClient, modifiers: u16) {
    client
        .send(
            inputs_client::KEY_MODIFIERS,
            &body(&KeyModifiers { modifiers }),
        )
        .await;
}

/// Wait until the platform has recorded events that satisfy `done`, and
/// return them all, leaving none recorded.
async fn wait_for_events(
    server: &TestServer,
    done: impl Fn(&[InputEvent]) -> bool,
) -> Vec<InputEvent> {
    let mut events = Vec::new();
    tokio::time::timeout(WAIT, async {
        loop {
            events.extend(server.platform.take_input_events().unwrap());
            if done(&events) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the expected events never arrived; got {events:?}"));
    events
}

/// Send the sentinel, wait for it, and return what the server injected
/// before it.
async fn settle(server: &TestServer, client: &mut TestClient) -> Vec<InputEvent> {
    send_key(client, inputs_client::KEY_DOWN, SENTINEL).await;
    send_key(client, inputs_client::KEY_UP, SENTINEL | 0x80).await;
    let release = key(SENTINEL as u16, false);
    let mut events = wait_for_events(server, |events| events.last() == Some(&release)).await;
    let sentinel = events.split_off(events.len() - 2);
    assert_eq!(sentinel, vec![key(SENTINEL as u16, true), release]);
    events
}

async fn expect_quiet(client: &mut TestClient) {
    if let Some(message) = client.recv_within(QUIET).await {
        panic!(
            "expected nothing, but got opcode {}",
            message.header.message_type
        );
    }
}

async fn expect_motion_ack(client: &mut TestClient) {
    let message = client.recv().await;
    assert_eq!(message.header.message_type, inputs_server::MOUSE_MOTION_ACK);
    assert!(message.payload().is_empty());
}

fn set_mouse_mode(server: &TestServer, mode: MouseMode) {
    server
        .handle
        .session()
        .unwrap()
        .mouse_mode()
        .send_replace(mode);
}

#[tokio::test]
async fn init_carries_the_lock_state() {
    let server = TestServer::start().await;
    let (_main, _inputs, modifiers) = open_inputs(&server).await;
    assert_eq!(modifiers, 0);

    server
        .platform
        .force_lock_state(LockState {
            caps: true,
            num: false,
            scroll: true,
        })
        .unwrap();
    let (_main, _inputs, modifiers) = open_inputs(&server).await;
    assert_eq!(
        modifiers,
        keyboard_modifiers::CAPS_LOCK | keyboard_modifiers::SCROLL_LOCK
    );
}

#[tokio::test]
async fn keys_become_evdev_codes() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    send_key(&mut inputs, inputs_client::KEY_DOWN, 0x1E).await;
    send_key(&mut inputs, inputs_client::KEY_UP, 0x9E).await;
    // Left, as ryll's make_scancode puts it on the wire.
    send_key(&mut inputs, inputs_client::KEY_DOWN, 0x4BE0).await;
    send_key(&mut inputs, inputs_client::KEY_UP, 0xCBE0).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![
            key(30, true),
            key(30, false),
            key(105, true),
            key(105, false)
        ]
    );
}

#[tokio::test]
async fn key_scancode_bytes_decode_across_messages() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    let scancode = |codes: &[u8]| {
        body(&KeyScancode {
            codes: codes.to_vec(),
        })
    };
    inputs
        .send(inputs_client::KEY_SCANCODE, &scancode(&[0xE0]))
        .await;
    inputs
        .send(inputs_client::KEY_SCANCODE, &scancode(&[0x48, 0xE0, 0xC8]))
        .await;
    // An unmapped code is dropped, and what follows still works.
    inputs
        .send(inputs_client::KEY_SCANCODE, &scancode(&[0x59, 0x1F, 0x9F]))
        .await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![
            key(103, true),
            key(103, false),
            key(31, true),
            key(31, false)
        ]
    );
}

#[tokio::test]
async fn key_modifiers_press_the_locks_that_differ() {
    let server = TestServer::start().await;
    let (_main, mut inputs, modifiers) = open_inputs(&server).await;
    assert_eq!(modifiers, 0);

    send_modifiers(&mut inputs, keyboard_modifiers::NUM_LOCK).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![key(69, true), key(69, false)]
    );
    assert!(server.platform.platform().input.lock_state().unwrap().num);

    // The same state again, as ryll's keepalive sends it: nothing.
    send_modifiers(&mut inputs, keyboard_modifiers::NUM_LOCK).await;
    assert_eq!(settle(&server, &mut inputs).await, vec![]);

    // Two locks differ: both are toggled.
    send_modifiers(
        &mut inputs,
        keyboard_modifiers::CAPS_LOCK | keyboard_modifiers::SCROLL_LOCK,
    )
    .await;
    let events = settle(&server, &mut inputs).await;
    assert_eq!(events.len(), 6, "{events:?}");
    assert_eq!(
        server.platform.platform().input.lock_state().unwrap(),
        LockState {
            caps: true,
            num: false,
            scroll: true
        }
    );
}

#[tokio::test]
async fn key_modifiers_leave_a_held_lock_key_alone() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    // The client holds Caps Lock, which turns it on.
    send_key(&mut inputs, inputs_client::KEY_DOWN, 0x3A).await;
    assert_eq!(settle(&server, &mut inputs).await, vec![key(58, true)]);

    // While it is held, a KEY_MODIFIERS without caps does not toggle it.
    send_modifiers(&mut inputs, 0).await;
    assert_eq!(settle(&server, &mut inputs).await, vec![]);
    assert!(server.platform.platform().input.lock_state().unwrap().caps);

    // Once it is released, it does.
    send_key(&mut inputs, inputs_client::KEY_UP, 0xBA).await;
    send_modifiers(&mut inputs, 0).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![key(58, false), key(58, true), key(58, false)]
    );
    assert!(!server.platform.platform().input.lock_state().unwrap().caps);
}

#[tokio::test]
async fn positions_are_clamped_to_the_desktop() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    send_position(&mut inputs, 10, 20, 0).await;
    send_position(&mut inputs, 1000, 1000, 0).await;
    send_position(&mut inputs, 255, 191, 0).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![
            InputEvent::PointerAbsolute { x: 10, y: 20 },
            InputEvent::PointerAbsolute { x: 255, y: 191 },
            InputEvent::PointerAbsolute { x: 255, y: 191 },
        ]
    );

    // After a resize, the clamp follows within its maximum age.
    server
        .platform
        .set_size(Size {
            width: 320,
            height: 200,
        })
        .unwrap();
    tokio::time::sleep(GEOMETRY_MAX_AGE + Duration::from_millis(50)).await;
    send_position(&mut inputs, 1000, 1000, 0).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![InputEvent::PointerAbsolute { x: 319, y: 199 }]
    );
}

#[tokio::test]
async fn a_position_on_another_display_is_ignored() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    send_position(&mut inputs, 10, 20, 1).await;
    send_position(&mut inputs, 30, 40, 1).await;
    send_position(&mut inputs, 50, 60, 0).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![InputEvent::PointerAbsolute { x: 50, y: 60 }]
    );
}

/// Andris follows spice-server, which applies `MOUSE_POSITION` only in
/// client mouse mode and `MOUSE_MOTION` only in server mouse mode.
#[tokio::test]
async fn each_pointer_message_applies_only_in_its_mouse_mode() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    // Client mode, the default: motion is ignored.
    send_motion(&mut inputs, 5, -3).await;
    send_position(&mut inputs, 10, 20, 0).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![InputEvent::PointerAbsolute { x: 10, y: 20 }]
    );

    // Server mode: position is ignored, and motion is relative.
    set_mouse_mode(&server, MouseMode::Server);
    send_position(&mut inputs, 30, 40, 0).await;
    send_motion(&mut inputs, 5, -3).await;
    send_motion(&mut inputs, -2000, 7).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![
            InputEvent::PointerRelative { dx: 5, dy: -3 },
            InputEvent::PointerRelative { dx: -2000, dy: 7 },
        ]
    );
}

#[tokio::test]
async fn buttons_map_and_wheel_is_four_and_five() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    for id in [mouse_button_id::UP, mouse_button_id::DOWN] {
        send_button(&mut inputs, inputs_client::MOUSE_PRESS, id).await;
        send_button(&mut inputs, inputs_client::MOUSE_RELEASE, id).await;
    }
    send_button(
        &mut inputs,
        inputs_client::MOUSE_PRESS,
        mouse_button_id::LEFT,
    )
    .await;
    send_button(
        &mut inputs,
        inputs_client::MOUSE_RELEASE,
        mouse_button_id::LEFT,
    )
    .await;
    // An unknown id is ignored.
    send_button(&mut inputs, inputs_client::MOUSE_PRESS, 9).await;
    assert_eq!(
        settle(&server, &mut inputs).await,
        vec![
            button(Button::WheelUp, true),
            button(Button::WheelUp, false),
            button(Button::WheelDown, true),
            button(Button::WheelDown, false),
            button(Button::Left, true),
            button(Button::Left, false),
        ]
    );
}

#[tokio::test]
async fn every_fourth_motion_or_position_is_acked() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    for i in 0..3 {
        send_position(&mut inputs, i, i, 0).await;
    }
    expect_quiet(&mut inputs).await;
    send_position(&mut inputs, 3, 3, 0).await;
    expect_motion_ack(&mut inputs).await;

    // Messages that are ignored count too: the client throttles on acks.
    send_motion(&mut inputs, 1, 1).await;
    send_position(&mut inputs, 4, 4, 1).await;
    send_position(&mut inputs, 5, 5, 0).await;
    expect_quiet(&mut inputs).await;
    send_motion(&mut inputs, 1, 1).await;
    expect_motion_ack(&mut inputs).await;
    expect_quiet(&mut inputs).await;
}

#[tokio::test]
async fn closing_the_channel_releases_held_keys_and_buttons() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;

    send_key(&mut inputs, inputs_client::KEY_DOWN, 0x1E).await;
    send_key(&mut inputs, inputs_client::KEY_DOWN, 0x1F).await;
    send_key(&mut inputs, inputs_client::KEY_UP, 0x9F).await;
    send_button(
        &mut inputs,
        inputs_client::MOUSE_PRESS,
        mouse_button_id::LEFT,
    )
    .await;
    settle(&server, &mut inputs).await;

    drop(inputs);
    let events = wait_for_events(&server, |events| events.len() >= 2).await;
    assert_eq!(
        events,
        vec![key(30, false), button(Button::Left, false)],
        "only what was still held is released"
    );
}

#[tokio::test]
async fn replacing_the_session_releases_held_keys_and_buttons() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;
    let old = server.session_id();

    send_key(&mut inputs, inputs_client::KEY_DOWN, 0x4BE0).await;
    send_button(
        &mut inputs,
        inputs_client::MOUSE_PRESS,
        mouse_button_id::RIGHT,
    )
    .await;
    settle(&server, &mut inputs).await;

    // A new client's main link replaces the session, which drops the old
    // inputs channel's handler without the client closing anything.
    let (_new_main, new) = server.open_main().await;
    assert_ne!(new, old);
    let events = wait_for_events(&server, |events| events.len() >= 2).await;
    assert_eq!(events, vec![key(105, false), button(Button::Right, false)]);
    inputs.expect_closed().await;
}

#[tokio::test]
async fn a_malformed_key_event_closes_the_channel() {
    let server = TestServer::start().await;
    let (_main, mut inputs, _) = open_inputs(&server).await;
    inputs.send(inputs_client::KEY_DOWN, &[0x1E, 0]).await;
    inputs.expect_closed().await;
}
