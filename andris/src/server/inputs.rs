//! The inputs channel.
//!
//! On link andris sends `INPUTS_INIT` with the desktop's lock state, then
//! turns what the client sends into [`InputEvent`]s for the platform's
//! [`InputSink`]. Mapping SPICE onto the platform's types (scancodes,
//! button ids, modifier flags) happens here, so the platform knows
//! nothing of SPICE.
//!
//! # Keys
//!
//! `KEY_DOWN`, `KEY_UP` and `KEY_SCANCODE` carry set-1 bytes, which
//! [`KeyDecoder`] turns into evdev key strokes. Whether a byte is a press
//! or a release is its own break bit, not which message carried it.
//!
//! `KEY_MODIFIERS` names the lock keys the client wants on. For each lock
//! that differs from the desktop's, andris presses and releases that lock
//! key, as spice-server does. A lock whose key the client is holding is
//! left alone, also as spice-server does. Ryll sends `KEY_MODIFIERS` as
//! an idle keepalive, so an unchanged state injects nothing.
//!
//! # The pointer
//!
//! Following spice-server, `MOUSE_POSITION` (absolute) is applied only
//! in client mouse mode, and `MOUSE_MOTION` (relative) only in server
//! mouse mode; each is ignored in the other mode. A position must name
//! display 0, the only one andris serves, and is clamped to the desktop.
//! Every motion and position message counts towards
//! `MOUSE_MOTION_ACK`, which is sent after every [`MOTION_ACK_BUNCH`]th,
//! whether or not the message was applied: the client throttles its
//! pointer on those acks.
//!
//! Buttons are pressed and released by their `MOUSE_PRESS` and
//! `MOUSE_RELEASE` id. The button masks in the motion and position
//! messages are not used.
//!
//! # The desktop's size
//!
//! Clamping needs the desktop's size, which may block to read. The
//! channel reads it when it opens and keeps it, reading it again before a
//! clamp once it is [`GEOMETRY_MAX_AGE`] old, so a resize reaches the
//! clamp within that age.
//!
//! # Closing
//!
//! A [`HeldInput`] guard tracks the keys and buttons pressed and not yet
//! released. Its `Drop` releases them, so they are released however the
//! channel ends: the client closing it, an error, or the session being
//! cancelled when a new client replaces it, which drops this handler.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use shakenfist_spice_protocol::constants::{keyboard_modifiers, mouse_button_id};
use shakenfist_spice_protocol::inputs_client;
use shakenfist_spice_protocol::messages::{
    InputsInit, KeyEvent, KeyModifiers, KeyScancode, MouseButton, MouseMotion, MousePosition,
    ReceivedMessage, WireType,
};
use tracing::{debug, warn};

use super::channel::{ChannelConn, ChannelContext};
use super::keymap::{KeyDecoder, KeyStroke};
use super::opcodes::InputsOpcode;
use super::session::MouseMode;
use crate::platform::{Button, EvdevCode, InputEvent, InputSink, LockState, Size};

/// How many motion and position messages each `MOUSE_MOTION_ACK`
/// answers, as spice-server and ryll agree.
pub const MOTION_ACK_BUNCH: u32 = 4;

/// How old the cached desktop size may be before a clamp reads it again.
pub const GEOMETRY_MAX_AGE: Duration = Duration::from_millis(250);

/// Serve an inputs channel until the client closes it.
pub async fn run(ctx: ChannelContext, mut conn: ChannelConn<InputsOpcode>) -> Result<()> {
    let sink = ctx.platform.input.clone();
    let locks = blocking(&sink, |sink| sink.lock_state())
        .await
        .context("reading the lock state")?;
    let size = read_size(&ctx).await?;
    conn.send_msg(
        InputsOpcode::Init,
        &InputsInit {
            keyboard_modifiers: modifiers_from_locks(locks),
        },
    )
    .await?;

    let mut inputs = Inputs {
        held: HeldInput::new(sink),
        keys: KeyDecoder::new(),
        size,
        size_read: Instant::now(),
        motions: 0,
        logged: HashSet::new(),
        ctx,
    };
    while let Some(message) = conn.recv().await? {
        if inputs.handle(&mut conn, &message).await? {
            conn.send(InputsOpcode::MouseMotionAck, &[]).await?;
        }
    }
    Ok(())
}

/// Run `f` with the sink on the blocking pool.
async fn blocking<T, F>(sink: &Arc<dyn InputSink>, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&dyn InputSink) -> Result<T> + Send + 'static,
{
    let sink = sink.clone();
    tokio::task::spawn_blocking(move || f(sink.as_ref()))
        .await
        .context("the input task failed")?
}

/// Read the desktop's size on the blocking pool.
async fn read_size(ctx: &ChannelContext) -> Result<Size> {
    let geometry = ctx.platform.geometry.clone();
    tokio::task::spawn_blocking(move || geometry.size())
        .await
        .context("the geometry task failed")?
        .context("reading the desktop's size")
}

/// The `keyboard_modifiers` flags for `locks`.
pub fn modifiers_from_locks(locks: LockState) -> u16 {
    let mut flags = 0;
    if locks.scroll {
        flags |= keyboard_modifiers::SCROLL_LOCK;
    }
    if locks.num {
        flags |= keyboard_modifiers::NUM_LOCK;
    }
    if locks.caps {
        flags |= keyboard_modifiers::CAPS_LOCK;
    }
    flags
}

/// The lock state `keyboard_modifiers` flags name. Other bits are
/// ignored.
pub fn locks_from_modifiers(flags: u16) -> LockState {
    LockState {
        caps: flags & keyboard_modifiers::CAPS_LOCK != 0,
        num: flags & keyboard_modifiers::NUM_LOCK != 0,
        scroll: flags & keyboard_modifiers::SCROLL_LOCK != 0,
    }
}

/// The button a `mouse_button_id` names.
pub fn button_from_id(id: u8) -> Option<Button> {
    match id {
        mouse_button_id::LEFT => Some(Button::Left),
        mouse_button_id::MIDDLE => Some(Button::Middle),
        mouse_button_id::RIGHT => Some(Button::Right),
        mouse_button_id::UP => Some(Button::WheelUp),
        mouse_button_id::DOWN => Some(Button::WheelDown),
        mouse_button_id::SIDE => Some(Button::Side),
        mouse_button_id::EXTRA => Some(Button::Extra),
        _ => None,
    }
}

/// The keys and buttons a channel has pressed and not released, and the
/// sink it pressed them on. Dropping it releases them.
pub struct HeldInput {
    sink: Arc<dyn InputSink>,
    keys: BTreeSet<EvdevCode>,
    buttons: Vec<Button>,
}

impl HeldInput {
    pub fn new(sink: Arc<dyn InputSink>) -> Self {
        Self {
            sink,
            keys: BTreeSet::new(),
            buttons: Vec::new(),
        }
    }

    /// Inject a key stroke, and track it.
    pub fn key(&mut self, stroke: KeyStroke) -> Result<()> {
        self.sink.inject(InputEvent::Key {
            code: stroke.code,
            pressed: stroke.pressed,
        })?;
        if stroke.pressed {
            self.keys.insert(stroke.code);
        } else {
            self.keys.remove(&stroke.code);
        }
        Ok(())
    }

    /// Inject a button press or release, and track it.
    pub fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        self.sink.inject(InputEvent::Button { button, pressed })?;
        if pressed {
            if !self.buttons.contains(&button) {
                self.buttons.push(button);
            }
        } else {
            self.buttons.retain(|&held| held != button);
        }
        Ok(())
    }

    /// Inject pointer motion, which holds nothing.
    pub fn pointer(&self, event: InputEvent) -> Result<()> {
        self.sink.inject(event)
    }

    /// Whether `code` is held.
    pub fn is_held(&self, code: EvdevCode) -> bool {
        self.keys.contains(&code)
    }

    /// The sink, for calls that do not press anything.
    pub fn sink(&self) -> &Arc<dyn InputSink> {
        &self.sink
    }
}

impl Drop for HeldInput {
    fn drop(&mut self) {
        // InputSink::inject_release only queues, so this cannot block, and
        // it has room even when a flood has filled the sink's queue.
        let keys = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|code| InputEvent::Key {
                code,
                pressed: false,
            });
        let buttons =
            std::mem::take(&mut self.buttons)
                .into_iter()
                .map(|button| InputEvent::Button {
                    button,
                    pressed: false,
                });
        for event in keys.chain(buttons) {
            debug!(?event, "releasing input held when the channel closed");
            if let Err(e) = self.sink.inject_release(event) {
                warn!(
                    error = format!("{e:#}"),
                    "releasing held input failed; the desktop may still hold it"
                );
                return;
            }
        }
    }
}

/// Something logged once per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Once {
    OtherDisplay,
    PositionInServerMode,
    MotionInClientMode,
    UnknownButton(u8),
}

/// One inputs channel's state.
struct Inputs {
    ctx: ChannelContext,
    held: HeldInput,
    keys: KeyDecoder,
    /// The desktop's size as last read, and when.
    size: Size,
    size_read: Instant,
    /// Motion and position messages since the last `MOUSE_MOTION_ACK`.
    motions: u32,
    logged: HashSet<Once>,
}

impl Inputs {
    /// Handle one message from the client. True if a
    /// `MOUSE_MOTION_ACK` is due.
    async fn handle(
        &mut self,
        conn: &mut ChannelConn<InputsOpcode>,
        message: &ReceivedMessage,
    ) -> Result<bool> {
        match message.header.message_type {
            inputs_client::KEY_DOWN | inputs_client::KEY_UP => {
                let key = KeyEvent::decode(message.payload()).context("a malformed key event")?;
                let strokes = self.keys.key_event(key.scancode);
                self.strokes(strokes)?;
            }
            inputs_client::KEY_SCANCODE => {
                let codes =
                    KeyScancode::decode(message.payload()).context("a malformed KEY_SCANCODE")?;
                let strokes = self.keys.bytes(&codes.codes);
                self.strokes(strokes)?;
            }
            inputs_client::KEY_MODIFIERS => {
                let modifiers =
                    KeyModifiers::decode(message.payload()).context("a malformed KEY_MODIFIERS")?;
                self.set_locks(locks_from_modifiers(modifiers.modifiers))
                    .await?;
            }
            inputs_client::MOUSE_POSITION => {
                let position = MousePosition::decode(message.payload())
                    .context("a malformed MOUSE_POSITION")?;
                self.position(position).await?;
                return Ok(self.count_motion());
            }
            inputs_client::MOUSE_MOTION => {
                let motion =
                    MouseMotion::decode(message.payload()).context("a malformed MOUSE_MOTION")?;
                self.motion(motion)?;
                return Ok(self.count_motion());
            }
            inputs_client::MOUSE_PRESS | inputs_client::MOUSE_RELEASE => {
                let pressed = message.header.message_type == inputs_client::MOUSE_PRESS;
                let button = MouseButton::decode(message.payload())
                    .context("a malformed mouse button message")?;
                match button_from_id(button.button) {
                    Some(mapped) => self.held.button(mapped, pressed)?,
                    None => self.once(Once::UnknownButton(button.button), || {
                        format!("ignoring unknown mouse button id {}", button.button)
                    }),
                }
            }
            _ => conn.ignore(message),
        }
        Ok(false)
    }

    fn strokes(&mut self, strokes: Vec<KeyStroke>) -> Result<()> {
        for stroke in strokes {
            self.held.key(stroke)?;
        }
        Ok(())
    }

    /// Count a motion or position message; true if an ack is due.
    fn count_motion(&mut self) -> bool {
        self.motions += 1;
        if self.motions >= MOTION_ACK_BUNCH {
            self.motions = 0;
            true
        } else {
            false
        }
    }

    fn mouse_mode(&self) -> MouseMode {
        *self.ctx.session.mouse_mode().borrow()
    }

    async fn position(&mut self, position: MousePosition) -> Result<()> {
        if position.display_id != 0 {
            self.once(Once::OtherDisplay, || {
                format!(
                    "ignoring MOUSE_POSITION on display {}; andris serves only display 0",
                    position.display_id
                )
            });
            return Ok(());
        }
        if self.mouse_mode() != MouseMode::Client {
            self.once(Once::PositionInServerMode, || {
                "ignoring MOUSE_POSITION in server mouse mode".to_string()
            });
            return Ok(());
        }
        if self.size_read.elapsed() >= GEOMETRY_MAX_AGE {
            self.size = read_size(&self.ctx).await?;
            self.size_read = Instant::now();
        }
        // Size::check holds both sides to at least 1.
        let x = position.x.min(self.size.width.saturating_sub(1));
        let y = position.y.min(self.size.height.saturating_sub(1));
        self.held.pointer(InputEvent::PointerAbsolute { x, y })
    }

    fn motion(&mut self, motion: MouseMotion) -> Result<()> {
        if self.mouse_mode() != MouseMode::Server {
            self.once(Once::MotionInClientMode, || {
                "ignoring MOUSE_MOTION in client mouse mode".to_string()
            });
            return Ok(());
        }
        self.held.pointer(InputEvent::PointerRelative {
            dx: motion.dx,
            dy: motion.dy,
        })
    }

    /// Make the desktop's locks `wanted`, except any whose key the client
    /// holds.
    async fn set_locks(&mut self, wanted: LockState) -> Result<()> {
        let held = LockState {
            caps: self.held.is_held(EvdevCode::CAPSLOCK),
            num: self.held.is_held(EvdevCode::NUMLOCK),
            scroll: self.held.is_held(EvdevCode::SCROLLLOCK),
        };
        blocking(self.held.sink(), move |sink| {
            let current = sink.lock_state()?;
            let wanted = LockState {
                caps: if held.caps { current.caps } else { wanted.caps },
                num: if held.num { current.num } else { wanted.num },
                scroll: if held.scroll {
                    current.scroll
                } else {
                    wanted.scroll
                },
            };
            if wanted != current {
                sink.set_lock_state(wanted)?;
            }
            Ok(())
        })
        .await
        .context("setting the lock state")
    }

    /// Log what `what` says, at debug level, the first time `key`
    /// happens on this channel.
    fn once(&mut self, key: Once, what: impl FnOnce() -> String) {
        if self.logged.insert(key) {
            debug!(
                session = self.ctx.session.id(),
                "{} (logged once per channel)",
                what()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::synthetic::SyntheticPlatform;

    #[test]
    fn modifier_flags_round_trip() {
        for bits in 0..8u16 {
            assert_eq!(modifiers_from_locks(locks_from_modifiers(bits)), bits);
        }
        assert_eq!(
            locks_from_modifiers(keyboard_modifiers::CAPS_LOCK | 0x8000),
            LockState {
                caps: true,
                num: false,
                scroll: false
            },
            "unknown bits are ignored"
        );
    }

    #[test]
    fn button_ids_map_per_decision_12() {
        assert_eq!(button_from_id(1), Some(Button::Left));
        assert_eq!(button_from_id(2), Some(Button::Middle));
        assert_eq!(button_from_id(3), Some(Button::Right));
        assert_eq!(button_from_id(4), Some(Button::WheelUp));
        assert_eq!(button_from_id(5), Some(Button::WheelDown));
        assert_eq!(button_from_id(6), Some(Button::Side));
        assert_eq!(button_from_id(7), Some(Button::Extra));
        assert_eq!(button_from_id(0), None);
        assert_eq!(button_from_id(8), None);
    }

    #[test]
    fn dropping_held_input_releases_only_what_is_held() {
        let platform = SyntheticPlatform::new(Size {
            width: 64,
            height: 64,
        })
        .unwrap();
        let mut held = HeldInput::new(platform.platform().input);
        let stroke = |code, pressed| KeyStroke {
            code: EvdevCode(code),
            pressed,
        };
        held.key(stroke(30, true)).unwrap();
        held.key(stroke(31, true)).unwrap();
        held.key(stroke(31, false)).unwrap();
        held.button(Button::Left, true).unwrap();
        held.button(Button::Right, true).unwrap();
        held.button(Button::Right, false).unwrap();
        platform.take_input_events().unwrap();
        drop(held);
        assert_eq!(
            platform.take_input_events().unwrap(),
            vec![
                InputEvent::Key {
                    code: EvdevCode(30),
                    pressed: false
                },
                InputEvent::Button {
                    button: Button::Left,
                    pressed: false
                },
            ]
        );
    }
}
