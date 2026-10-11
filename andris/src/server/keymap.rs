//! SPICE's key codes: PC AT scan code set 1, decoded to evdev codes.
//!
//! A SPICE client sends set-1 bytes. `KEY_DOWN` and `KEY_UP` carry up to
//! four of them in a 32-bit code, least significant byte first, ending at
//! the first zero byte; `KEY_SCANCODE` carries any number. Both feed the
//! same [`KeyDecoder`], one per inputs channel, so an `0xE0` prefix that
//! ends one message applies to the first byte of the next, as it would
//! on a real keyboard.
//!
//! A byte with `0x80` set is a release (break code) of the key the other
//! seven bits name. After `0xE0` the byte names an extended key, which
//! [`extended_to_evdev`] maps. `0xE1` starts the three-byte Pause
//! sequence (`E1 1D 45` to press, `E1 9D C5` to release); decoding it
//! as plain bytes would press Ctrl and toggle Num Lock instead.
//!
//! Keyboards wrap some extended keys, such as Print Screen and the
//! navigation keys, in "fake shifts": `E0 2A` or `E0 36` before the key
//! and `E0 AA` or `E0 B6` after it, depending on the Shift and Num Lock
//! state. Clients that pass hardware scancodes through, such as
//! spice-gtk, can send them. They are dropped without a warning, since
//! the shift keys themselves arrive unprefixed.
//!
//! Any other code with no mapping is dropped, with a warning logged once
//! per code for each decoder.

use std::collections::HashSet;

use tracing::warn;

use crate::platform::EvdevCode;

/// The prefix of an extended key.
const PREFIX_E0: u8 = 0xE0;
/// The prefix of the Pause sequence.
const PREFIX_E1: u8 = 0xE1;
/// The break (release) bit.
const BREAK: u8 = 0x80;
/// The fake left and right shifts, which follow `0xE0`.
const FAKE_SHIFTS: [u8; 2] = [0x2A, 0x36];

/// `KEY_PAUSE`.
const KEY_PAUSE: EvdevCode = EvdevCode(119);

/// One key pressed or released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    pub code: EvdevCode,
    pub pressed: bool,
}

/// The evdev code of the set-1 key `code` (its break bit clear), with no
/// prefix. Codes 0x01 to 0x53, and 0x56 to 0x58, are the same in both.
pub fn plain_to_evdev(code: u8) -> Option<EvdevCode> {
    matches!(code, 0x01..=0x53 | 0x56..=0x58).then_some(EvdevCode(u16::from(code)))
}

/// The evdev code of the set-1 key `code` (its break bit clear) after an
/// `0xE0` prefix.
pub fn extended_to_evdev(code: u8) -> Option<EvdevCode> {
    let evdev = match code {
        0x1C => 96,  // KPENTER
        0x1D => 97,  // RIGHTCTRL
        0x35 => 98,  // KPSLASH
        0x37 => 99,  // SYSRQ
        0x38 => 100, // RIGHTALT
        0x47 => 102, // HOME
        0x48 => 103, // UP
        0x49 => 104, // PAGEUP
        0x4B => 105, // LEFT
        0x4D => 106, // RIGHT
        0x4F => 107, // END
        0x50 => 108, // DOWN
        0x51 => 109, // PAGEDOWN
        0x52 => 110, // INSERT
        0x53 => 111, // DELETE
        0x5B => 125, // LEFTMETA
        0x5C => 126, // RIGHTMETA
        0x5D => 127, // COMPOSE
        _ => return None,
    };
    Some(EvdevCode(evdev))
}

/// Where the decoder is in a multi-byte sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefix {
    None,
    /// After `0xE0`.
    E0,
    /// After `0xE1`, holding the sequence's second byte once it arrives.
    E1(Option<u8>),
}

/// Decodes set-1 bytes into key strokes, keeping prefixes across calls.
#[derive(Debug)]
pub struct KeyDecoder {
    prefix: Prefix,
    /// The unmapped codes already logged, as the prefix byte (or 0) in
    /// the high byte and the code in the low.
    logged: HashSet<u16>,
}

impl Default for KeyDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self {
            prefix: Prefix::None,
            logged: HashSet::new(),
        }
    }

    /// Decode a `KEY_DOWN` or `KEY_UP` code: its bytes least significant
    /// first, up to the first zero byte.
    pub fn key_event(&mut self, code: u32) -> Vec<KeyStroke> {
        let bytes: Vec<u8> = code
            .to_le_bytes()
            .into_iter()
            .take_while(|&byte| byte != 0)
            .collect();
        self.bytes(&bytes)
    }

    /// Decode raw set-1 bytes, as `KEY_SCANCODE` carries them.
    pub fn bytes(&mut self, bytes: &[u8]) -> Vec<KeyStroke> {
        bytes.iter().filter_map(|&byte| self.byte(byte)).collect()
    }

    /// Decode one byte; `None` if it completes no key stroke.
    fn byte(&mut self, byte: u8) -> Option<KeyStroke> {
        match self.prefix {
            Prefix::E1(None) => {
                self.prefix = Prefix::E1(Some(byte));
                None
            }
            Prefix::E1(Some(first)) => {
                self.prefix = Prefix::None;
                match (first, byte) {
                    (0x1D, 0x45) => Some(KeyStroke {
                        code: KEY_PAUSE,
                        pressed: true,
                    }),
                    (0x9D, 0xC5) => Some(KeyStroke {
                        code: KEY_PAUSE,
                        pressed: false,
                    }),
                    _ => {
                        self.unmapped(u16::from_be_bytes([first, byte]), "E1 sequence");
                        None
                    }
                }
            }
            Prefix::E0 | Prefix::None if byte == PREFIX_E0 => {
                self.prefix = Prefix::E0;
                None
            }
            Prefix::E0 | Prefix::None if byte == PREFIX_E1 => {
                self.prefix = Prefix::E1(None);
                None
            }
            Prefix::E0 => {
                self.prefix = Prefix::None;
                let code = byte & !BREAK;
                if FAKE_SHIFTS.contains(&code) {
                    return None;
                }
                match extended_to_evdev(code) {
                    Some(evdev) => Some(KeyStroke {
                        code: evdev,
                        pressed: byte & BREAK == 0,
                    }),
                    None => {
                        self.unmapped(u16::from_be_bytes([PREFIX_E0, code]), "extended key");
                        None
                    }
                }
            }
            Prefix::None => {
                let code = byte & !BREAK;
                match plain_to_evdev(code) {
                    Some(evdev) => Some(KeyStroke {
                        code: evdev,
                        pressed: byte & BREAK == 0,
                    }),
                    None => {
                        self.unmapped(u16::from(code), "key");
                        None
                    }
                }
            }
        }
    }

    /// Log `code` the first time it is seen.
    fn unmapped(&mut self, code: u16, what: &str) {
        if self.logged.insert(code) {
            warn!(
                code = format!("{code:#06x}"),
                "an unmapped set-1 {what} was dropped (logged once per code)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: u16) -> KeyStroke {
        KeyStroke {
            code: EvdevCode(code),
            pressed: true,
        }
    }

    fn release(code: u16) -> KeyStroke {
        KeyStroke {
            code: EvdevCode(code),
            pressed: false,
        }
    }

    /// Ryll's `make_scancode` (shakenfist-spice-renderer
    /// channels/inputs.rs): an extended base is `0x1xx`, and goes on the
    /// wire as `(sc << 8) | 0xE0`.
    fn make_scancode(base: u32, release: bool) -> u32 {
        let code = if release { base | 0x80 } else { base };
        if base >= 0x100 {
            ((code & 0xFF) << 8) | 0xE0
        } else {
            code
        }
    }

    /// Every base code ryll's `scancode_for_logical_key` and
    /// `char_to_scancode` produce (shakenfist-spice-renderer
    /// channels/inputs.rs at 502199e), plus the Ctrl, Shift and Alt it
    /// presses around pastes and for held modifiers. Copied rather than
    /// imported: andris must not depend on the renderer.
    const RYLL_BASES: &[u32] = &[
        // Letters.
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C, // Digits.
        0x0B, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, // F1 to F12.
        0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58,
        // Space, Enter, Backspace, Tab, Escape.
        0x39, 0x1C, 0x0E, 0x0F, 0x01, // Delete, Insert, Home, End, PageUp, PageDown.
        0x153, 0x152, 0x147, 0x14F, 0x149, 0x151, // Up, Down, Left, Right.
        0x148, 0x150, 0x14B, 0x14D, // - = [ ] \ ; ' ` , . /
        0x0C, 0x0D, 0x1A, 0x1B, 0x2B, 0x27, 0x28, 0x29, 0x33, 0x34, 0x35,
        // Left Ctrl, Left Shift, Left Alt.
        0x1D, 0x2A, 0x38,
    ];

    #[test]
    fn every_code_ryll_sends_maps() {
        for &base in RYLL_BASES {
            let want = if base >= 0x100 {
                extended_to_evdev(base as u8).unwrap()
            } else {
                EvdevCode(base as u16)
            };
            let mut decoder = KeyDecoder::new();
            assert_eq!(
                decoder.key_event(make_scancode(base, false)),
                vec![KeyStroke {
                    code: want,
                    pressed: true
                }],
                "press of {base:#x}"
            );
            assert_eq!(
                decoder.key_event(make_scancode(base, true)),
                vec![KeyStroke {
                    code: want,
                    pressed: false
                }],
                "release of {base:#x}"
            );
            assert!(decoder.logged.is_empty(), "{base:#x} logged as unmapped");
        }
    }

    #[test]
    fn plain_codes_map_to_themselves() {
        let mut decoder = KeyDecoder::new();
        assert_eq!(decoder.key_event(0x1E), vec![press(30)]);
        assert_eq!(decoder.key_event(0x9E), vec![release(30)]);
        assert_eq!(decoder.key_event(0x56), vec![press(86)]);
        assert_eq!(decoder.key_event(0x58), vec![press(88)]);
        for code in [0x00, 0x54, 0x55, 0x59, 0x7F] {
            assert_eq!(plain_to_evdev(code), None, "{code:#x}");
        }
    }

    #[test]
    fn extended_codes_map_through_the_table() {
        let mut decoder = KeyDecoder::new();
        assert_eq!(decoder.key_event(0x4BE0), vec![press(105)]);
        assert_eq!(decoder.key_event(0xCBE0), vec![release(105)]);
        assert_eq!(decoder.key_event(0x5BE0), vec![press(125)]);
        assert_eq!(decoder.key_event(0x1CE0), vec![press(96)]);
    }

    #[test]
    fn a_key_event_stops_at_the_first_zero_byte() {
        let mut decoder = KeyDecoder::new();
        // Bytes 1E, 00, 30: the 30 after the zero is not read.
        assert_eq!(decoder.key_event(0x0030_001E), vec![press(30)]);
        assert_eq!(decoder.key_event(0), vec![]);
        // Several bytes in one code are all read.
        assert_eq!(decoder.key_event(0x9E1E), vec![press(30), release(30)]);
    }

    #[test]
    fn an_e0_prefix_carries_across_messages() {
        let mut decoder = KeyDecoder::new();
        assert_eq!(decoder.bytes(&[0xE0]), vec![]);
        assert_eq!(decoder.bytes(&[0x48, 0xE0]), vec![press(103)]);
        assert_eq!(decoder.key_event(0xC8), vec![release(103)]);
        // And the prefix is spent: a plain code follows.
        assert_eq!(decoder.key_event(0x48), vec![press(0x48)]);
    }

    #[test]
    fn scancode_bytes_decode_like_key_events() {
        let mut decoder = KeyDecoder::new();
        assert_eq!(
            decoder.bytes(&[0x1E, 0xE0, 0x4D, 0xE0, 0xCD, 0x9E]),
            vec![press(30), press(106), release(106), release(30)]
        );
    }

    #[test]
    fn unmapped_codes_are_dropped_and_logged_once() {
        let mut decoder = KeyDecoder::new();
        assert_eq!(decoder.bytes(&[0x59, 0xD9, 0x59]), vec![]);
        assert_eq!(decoder.logged.len(), 1);
        // An extended code with no mapping, then a mapped one: only the
        // unmapped one is dropped, and the prefix does not linger.
        assert_eq!(decoder.bytes(&[0xE0, 0x10, 0x1E]), vec![press(30)]);
        assert_eq!(decoder.logged.len(), 2);
        assert!(decoder.logged.contains(&0xE010));
    }

    #[test]
    fn fake_shifts_are_dropped_silently() {
        let mut decoder = KeyDecoder::new();
        // Print Screen as a keyboard sends it, wrapped in a fake left shift.
        assert_eq!(decoder.bytes(&[0xE0, 0x2A, 0xE0, 0x37]), vec![press(99)]);
        assert_eq!(decoder.bytes(&[0xE0, 0xB7, 0xE0, 0xAA]), vec![release(99)]);
        // Home with Num Lock on and right shift held: a fake right shift
        // release before it and press after, around the real right shift.
        assert_eq!(
            decoder.bytes(&[0x36, 0xE0, 0xB6, 0xE0, 0x47, 0xE0, 0xC7, 0xE0, 0x36, 0xB6]),
            vec![press(54), press(102), release(102), release(54)]
        );
        assert!(decoder.logged.is_empty(), "logged {:?}", decoder.logged);
    }

    #[test]
    fn pause_is_one_key_not_ctrl_and_num_lock() {
        let mut decoder = KeyDecoder::new();
        assert_eq!(decoder.bytes(&[0xE1, 0x1D, 0x45]), vec![press(119)]);
        assert_eq!(decoder.bytes(&[0xE1, 0x9D]), vec![]);
        assert_eq!(decoder.bytes(&[0xC5]), vec![release(119)]);
        // Any other E1 sequence is swallowed whole.
        assert_eq!(decoder.bytes(&[0xE1, 0x1E, 0x30, 0x1F]), vec![press(31)]);
    }
}
