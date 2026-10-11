#!/usr/bin/env python3
"""Generate andris's evdev-code-to-XKB-key-name table, and its test fixtures.

Andris injects keys as Linux evdev codes. The X11 backend turns an evdev code into an XKB key name using a table
made from xkeyboard-config's keycodes/evdev, where a key's evdev code is its keycode minus 8, and then turns the
name into a keycode using the names the X server itself reports. That second step is what makes a server with
xfree86 keycodes work.

This script writes:

- andris/src/platform/x11/evdev_names.rs, the table;
- andris/src/platform/x11/testdata/evdev.names and xfree86.names, the key names and aliases a server with each
  keycode set reports, for the unit tests.

Usage: scripts/gen-evdev-names.py [--xkb-dir /usr/share/X11/xkb] [--version VERSION]

The version defaults to what `dpkg-query -W xkb-data` reports. The keycodes files come from the xkb-data package
(xkeyboard-config), which is MIT licensed. Run it from anywhere; paths are relative to the repository root.
"""

import argparse
import os
import re
import subprocess
import sys


REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_DIR = os.path.join(REPO_ROOT, 'andris', 'src', 'platform', 'x11')
TESTDATA_DIR = os.path.join(OUT_DIR, 'testdata')

# The X protocol's keycodes are 8 to 255, and an evdev code is the evdev keycode minus 8.
EVDEV_OFFSET = 8
MIN_KEYCODE = 8
MAX_KEYCODE = 255

LICENCE_URL = 'https://gitlab.freedesktop.org/xkeyboard-config/xkeyboard-config/-/blob/master/COPYING'

SECTION_RE = re.compile(r'(default\s+)?xkb_keycodes\s+"([^"]+)"\s*\{(.*?)\n\s*\};', re.DOTALL)
KEY_RE = re.compile(r'^<([^<>]{1,4})>\s*=\s*(\d+)$')
ALIAS_RE = re.compile(r'^alias\s+<([^<>]{1,4})>\s*=\s*<([^<>]{1,4})>$')
INCLUDE_RE = re.compile(r'^include\s+"([^"(]+)\(([^")]+)\)"$')
IGNORED_RE = re.compile(r'^(minimum|maximum)\s*=\s*\d+$|^indicator\s+\d+\s*=\s*"[^"]*"$')


class Keycodes:
    """The key names and aliases one xkb_keycodes section defines, after its includes."""

    def __init__(self):
        self.names = {}
        self.aliases = {}
        self.skipped = []

    def set_key(self, name, keycode):
        """Give keycode `keycode` the name `name`; a later definition replaces an earlier one, as in xkbcomp.

        A keycode outside the X protocol's 8 to 255 is skipped: X cannot carry it, so no X server has that key.
        """
        if not MIN_KEYCODE <= keycode <= MAX_KEYCODE:
            self.skipped.append(name)
            return
        for other, code in list(self.names.items()):
            if code == keycode and other != name:
                print('note: keycode %d renamed from <%s> to <%s>' % (keycode, other, name), file=sys.stderr)
                del self.names[other]
        self.names[name] = keycode
        self.aliases.pop(name, None)

    def by_keycode(self):
        """The names, as a sorted list of (keycode, name)."""
        return sorted((code, name) for name, code in self.names.items())


def read_sections(path):
    """Every xkb_keycodes section in `path`, as {name: (is_default, [statement, ...])}."""
    with open(path) as f:
        text = f.read()
    text = re.sub(r'//[^\n]*', '', text)
    sections = {}
    for match in SECTION_RE.finditer(text):
        statements = []
        for line in match.group(3).split('\n'):
            for statement in line.split(';'):
                statement = statement.strip()
                if statement:
                    statements.append(statement)
        sections[match.group(2)] = (bool(match.group(1)), statements)
    if not sections:
        raise ValueError('%s has no xkb_keycodes sections' % path)
    return sections


def resolve(path, section=None, keycodes=None, depth=0):
    """The Keycodes of `section` in `path` (the default section if None), following includes in the same file."""
    if depth > 8:
        raise ValueError('includes in %s nest too deeply' % path)
    sections = read_sections(path)
    if section is None:
        defaults = [name for name, (is_default, _) in sections.items() if is_default]
        if len(defaults) != 1:
            raise ValueError('%s has %d default sections, not one' % (path, len(defaults)))
        section = defaults[0]
    if section not in sections:
        raise ValueError('%s has no section "%s"' % (path, section))
    if keycodes is None:
        keycodes = Keycodes()
    for statement in sections[section][1]:
        key = KEY_RE.match(statement)
        alias = ALIAS_RE.match(statement)
        include = INCLUDE_RE.match(statement)
        if key:
            keycodes.set_key(key.group(1), int(key.group(2)))
        elif alias:
            keycodes.aliases[alias.group(1)] = alias.group(2)
        elif include:
            if include.group(1) != os.path.basename(path):
                raise ValueError('%s includes another file, %s, which this script does not follow'
                                 % (path, include.group(1)))
            resolve(path, include.group(2), keycodes, depth + 1)
        elif not IGNORED_RE.match(statement):
            raise ValueError('%s: cannot parse "%s"' % (path, statement))
    return keycodes


def xkb_data_version():
    """The installed xkb-data package's version, as dpkg reports it."""
    out = subprocess.run(['dpkg-query', '-W', '-f=${Version}', 'xkb-data'], check=True, capture_output=True,
                         text=True)
    return out.stdout.strip()


def rust_name(name):
    """`name` as a Rust byte-string literal of exactly four bytes, padded with NULs."""
    return '*b"%s"' % (name + '\\0' * (4 - len(name)))


def write_table(path, keycodes, source, version):
    """Write evdev_names.rs: each evdev code with the XKB key name keycodes/evdev gives it."""
    lines = [
        '//! The XKB key name of each Linux evdev key code, generated by',
        '//! `scripts/gen-evdev-names.py`. Do not edit it by hand; run the script.',
        '//!',
        '//! Source: `%s`, from the `xkb-data` package' % source,
        '//! (xkeyboard-config) version %s. A name\'s evdev code is its keycode' % version,
        '//! there minus 8. Keys whose keycode is over 255 are left out, because',
        '//! the X protocol cannot carry them. xkeyboard-config is distributed',
        '//! under the MIT licence: <%s>.' % LICENCE_URL,
        '',
        '/// Evdev code and XKB key name, NUL-padded to four bytes, sorted by code.',
        '#[rustfmt::skip]',
        'pub(super) const EVDEV_NAMES: &[(u16, [u8; 4])] = &[',
    ]
    for keycode, name in keycodes.by_keycode():
        lines.append('    (%d, %s),' % (keycode - EVDEV_OFFSET, rust_name(name)))
    lines.append('];')
    with open(path, 'w') as f:
        f.write('\n'.join(lines) + '\n')


def write_fixture(path, keycodes, source, version):
    """Write a fixture: the key names and aliases a server using `source` reports through XKB GetNames."""
    lines = [
        '# The key names and aliases an X server whose keycodes come from',
        '# %s reports, generated by scripts/gen-evdev-names.py.' % source,
        '# Source: the xkb-data package (xkeyboard-config) version %s, under the MIT' % version,
        '# licence: %s' % LICENCE_URL,
        '#',
        '# Each line is "<keycode> <name>" or "alias <alias> <real name>".',
    ]
    for keycode, name in keycodes.by_keycode():
        lines.append('%d %s' % (keycode, name))
    # An X server drops an alias for a key it does not have.
    for alias, real in sorted(keycodes.aliases.items()):
        if real in keycodes.names:
            lines.append('alias %s %s' % (alias, real))
    with open(path, 'w') as f:
        f.write('\n'.join(lines) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    parser.add_argument('--xkb-dir', default='/usr/share/X11/xkb', help='the XKB data directory')
    parser.add_argument('--version', help='the xkb-data version to record (default: ask dpkg)')
    args = parser.parse_args()

    version = args.version or xkb_data_version()
    evdev_path = os.path.join(args.xkb_dir, 'keycodes', 'evdev')
    xfree86_path = os.path.join(args.xkb_dir, 'keycodes', 'xfree86')
    evdev = resolve(evdev_path)
    xfree86 = resolve(xfree86_path)

    os.makedirs(TESTDATA_DIR, exist_ok=True)
    write_table(os.path.join(OUT_DIR, 'evdev_names.rs'), evdev, evdev_path, version)
    write_fixture(os.path.join(TESTDATA_DIR, 'evdev.names'), evdev, 'keycodes/evdev', version)
    write_fixture(os.path.join(TESTDATA_DIR, 'xfree86.names'), xfree86, 'keycodes/xfree86', version)
    print('wrote %d evdev names, skipping %d over keycode 255; the xfree86 fixture has %d names'
          % (len(evdev.names), len(evdev.skipped), len(xfree86.names)))


if __name__ == '__main__':
    main()
