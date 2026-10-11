//! Turning evdev key codes into an X server's keycodes, by XKB key name.
//!
//! An X server's keycodes depend on how it was configured: one using
//! xkeyboard-config's `evdev` keycodes puts `<LEFT>` at 113, one using
//! `xfree86` keycodes puts it at 100. The XKB key *names* are the same in
//! both. So an evdev code becomes a name through [`EVDEV_NAMES`], which is
//! generated from xkeyboard-config's `keycodes/evdev`, and the name becomes
//! a keycode through the names the attached server reports with XKB
//! `GetNames`.

use std::collections::HashMap;

use super::evdev_names::EVDEV_NAMES;
use crate::platform::EvdevCode;

/// An XKB key name: up to four bytes, padded with NULs.
pub type KeyName = [u8; 4];

/// `name` with every byte after its first NUL made NUL, so that two
/// spellings of one name compare equal.
fn normalise(mut name: KeyName) -> KeyName {
    if let Some(end) = name.iter().position(|&b| b == 0) {
        name[end..].fill(0);
    }
    name
}

/// `name` as text, for logs.
pub fn display_name(name: &KeyName) -> String {
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    format!("<{}>", String::from_utf8_lossy(&name[..end]))
}

/// The XKB key name xkeyboard-config's `keycodes/evdev` gives `code`.
pub fn evdev_name(code: EvdevCode) -> Option<KeyName> {
    let index = EVDEV_NAMES
        .binary_search_by_key(&code.0, |&(c, _)| c)
        .ok()?;
    EVDEV_NAMES.get(index).map(|&(_, name)| name)
}

/// Why an evdev code has no keycode on this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unmapped {
    /// xkeyboard-config's `keycodes/evdev` names no key with this code.
    NoName,
    /// The key has this name, but the server has no key by that name.
    NotOnServer(KeyName),
}

/// Evdev codes to this server's keycodes, resolved once from the
/// server's key names.
#[derive(Debug, Clone)]
pub struct Keymap {
    keycodes: HashMap<u16, u8>,
}

impl Keymap {
    /// Resolve every evdev code the table names against the server's
    /// names, as XKB `GetNames` reports them: `names[i]` is the name of
    /// keycode `first_key + i`, and each alias is `(alias, real name)`.
    ///
    /// An empty name means the keycode has no key. If two keycodes share
    /// a name the lower wins. An alias that is also a real name is
    /// ignored, as XKB ignores it, and so is one whose real name the
    /// server lacks.
    pub fn new(first_key: u8, names: &[KeyName], aliases: &[(KeyName, KeyName)]) -> Self {
        let mut by_name: HashMap<KeyName, u8> = HashMap::new();
        for (keycode, name) in (first_key..=u8::MAX).zip(names) {
            let name = normalise(*name);
            if name != [0; 4] {
                by_name.entry(name).or_insert(keycode);
            }
        }
        let mut resolved_aliases = Vec::new();
        for (alias, real) in aliases {
            let alias = normalise(*alias);
            if by_name.contains_key(&alias) {
                continue;
            }
            if let Some(&keycode) = by_name.get(&normalise(*real)) {
                resolved_aliases.push((alias, keycode));
            }
        }
        for (alias, keycode) in resolved_aliases {
            by_name.entry(alias).or_insert(keycode);
        }
        let keycodes = EVDEV_NAMES
            .iter()
            .filter_map(|&(code, name)| Some((code, *by_name.get(&normalise(name))?)))
            .collect();
        Self { keycodes }
    }

    /// The server keycode for `code`, or why there is none.
    pub fn keycode(&self, code: EvdevCode) -> Result<u8, Unmapped> {
        if let Some(&keycode) = self.keycodes.get(&code.0) {
            return Ok(keycode);
        }
        match evdev_name(code) {
            Some(name) => Err(Unmapped::NotOnServer(name)),
            None => Err(Unmapped::NoName),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `KEY_A`, `KEY_LEFT` and `KEY_RIGHTALT`.
    const KEY_A: EvdevCode = EvdevCode(30);
    const KEY_LEFT: EvdevCode = EvdevCode(105);
    const KEY_RIGHTALT: EvdevCode = EvdevCode(100);

    fn name(text: &str) -> KeyName {
        let mut name = [0; 4];
        name[..text.len()].copy_from_slice(text.as_bytes());
        name
    }

    /// A fixture as `GetNames` would report it: the names from keycode 8
    /// to 255, and the aliases.
    fn parse_fixture(text: &str) -> (Vec<KeyName>, Vec<(KeyName, KeyName)>) {
        let mut names = vec![[0; 4]; 248];
        let mut aliases = Vec::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                [] => {}
                [first, ..] if first.starts_with('#') => {}
                ["alias", alias, real] => aliases.push((name(alias), name(real))),
                [keycode, key] => {
                    let keycode: usize = keycode.parse().unwrap();
                    names[keycode - 8] = name(key);
                }
                _ => panic!("bad fixture line {line:?}"),
            }
        }
        (names, aliases)
    }

    fn fixture_keymap(text: &str) -> Keymap {
        let (names, aliases) = parse_fixture(text);
        Keymap::new(8, &names, &aliases)
    }

    fn evdev_server() -> Keymap {
        fixture_keymap(include_str!("testdata/evdev.names"))
    }

    fn xfree86_server() -> Keymap {
        fixture_keymap(include_str!("testdata/xfree86.names"))
    }

    #[test]
    fn the_table_is_sorted_and_unique() {
        assert!(EVDEV_NAMES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(evdev_name(KEY_LEFT), Some(name("LEFT")));
        assert_eq!(evdev_name(EvdevCode(1)), Some(name("ESC")));
        assert_eq!(evdev_name(EvdevCode(0)), None);
    }

    #[test]
    fn left_resolves_under_both_keycode_sets() {
        assert_eq!(evdev_server().keycode(KEY_LEFT), Ok(113));
        assert_eq!(xfree86_server().keycode(KEY_LEFT), Ok(100));
    }

    #[test]
    fn keys_that_differ_between_the_sets() {
        // xfree86's 113 is right Alt, which is evdev's 108.
        assert_eq!(evdev_server().keycode(KEY_RIGHTALT), Ok(108));
        assert_eq!(xfree86_server().keycode(KEY_RIGHTALT), Ok(113));
        // The main block is the same in both.
        assert_eq!(evdev_server().keycode(KEY_A), Ok(38));
        assert_eq!(xfree86_server().keycode(KEY_A), Ok(38));
    }

    #[test]
    fn evdev_names_map_to_their_own_keycodes_on_an_evdev_server() {
        let keymap = evdev_server();
        for &(code, _) in EVDEV_NAMES {
            assert_eq!(
                keymap.keycode(EvdevCode(code)),
                Ok((code + 8) as u8),
                "evdev code {code}"
            );
        }
    }

    #[test]
    fn the_lock_keys_resolve_under_both_sets() {
        for keymap in [evdev_server(), xfree86_server()] {
            for code in [
                EvdevCode::CAPSLOCK,
                EvdevCode::NUMLOCK,
                EvdevCode::SCROLLLOCK,
            ] {
                assert!(keymap.keycode(code).is_ok(), "{code:?}");
            }
        }
    }

    #[test]
    fn unknown_codes_and_missing_names_are_reported() {
        let keymap = xfree86_server();
        assert_eq!(keymap.keycode(EvdevCode(0)), Err(Unmapped::NoName));
        assert_eq!(keymap.keycode(EvdevCode(4000)), Err(Unmapped::NoName));
        // Evdev code 112 is <I120>, which an xfree86 server lacks.
        assert_eq!(
            keymap.keycode(EvdevCode(112)),
            Err(Unmapped::NotOnServer(name("I120")))
        );
    }

    #[test]
    fn server_aliases_resolve_under_xfree86() {
        // KEY_MUTE is <MUTE>, which xfree86 has only as an alias of <I0D>.
        assert_eq!(evdev_server().keycode(EvdevCode(113)), Ok(121));
        assert_eq!(xfree86_server().keycode(EvdevCode(113)), Ok(141));
        // KEY_COMPOSE is <COMP>, an alias of xfree86's <MENU>.
        assert_eq!(xfree86_server().keycode(EvdevCode(127)), Ok(117));
    }

    #[test]
    fn aliases_resolve_to_their_real_keys() {
        // A server that has only <ALT2> at 50, aliased as <RALT>.
        let mut names = vec![[0; 4]; 248];
        names[50 - 8] = name("ALT2");
        let aliases = [
            (name("RALT"), name("ALT2")),
            // Neither of these may take effect.
            (name("ALT2"), name("ESC")),
            (name("LEFT"), name("NONE")),
        ];
        let keymap = Keymap::new(8, &names, &aliases);
        assert_eq!(keymap.keycode(KEY_RIGHTALT), Ok(50));
        assert_eq!(
            keymap.keycode(KEY_LEFT),
            Err(Unmapped::NotOnServer(name("LEFT")))
        );
    }

    #[test]
    fn names_compare_up_to_their_first_nul() {
        // <UP>, KEY_UP, with junk after its NUL.
        let mut names = vec![[0; 4]; 248];
        names[111 - 8] = *b"UP\0X";
        let keymap = Keymap::new(8, &names, &[]);
        assert_eq!(keymap.keycode(EvdevCode(103)), Ok(111));
        assert_eq!(normalise(*b"UP\0X"), *b"UP\0\0");
        assert_eq!(display_name(b"UP\0X"), "<UP>");
    }

    #[test]
    fn a_short_names_list_starts_at_first_key() {
        let names = [name("ESC"), name("AE01")];
        let keymap = Keymap::new(20, &names, &[]);
        assert_eq!(keymap.keycode(EvdevCode(1)), Ok(20));
        assert_eq!(keymap.keycode(EvdevCode(2)), Ok(21));
    }
}
