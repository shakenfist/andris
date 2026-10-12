#!/usr/bin/env python3
"""The X client the end-to-end test types into.

Maps a full-screen, override-redirect window filled with one known colour,
takes the input focus, and prints READY once the window is mapped. Then it
prints one line per KeyPress: the name of the key's unshifted keysym, as
XKeysymToString(XLookupKeysym(event, 0)) would give it (so 'a', 'Left').

With --detail, which the xrdp test bed's checks use, each KeyPress line is
instead

    KEY name=a shifted=A keycode=38 state=0x10

giving the unshifted and shifted keysyms of the event's keycode in its
group, the keycode, and the event's modifier and lock state (the core
protocol's state field, with the XKB group in bits 13-14). Every
MappingNotify is printed too, as

    MAPPING request=1 first=8 count=248

The keysyms are read from the core keyboard mapping, which the keylogger
fetches again on every MappingNotify. The X server sends one when the core
keyboard's keymap changes, including when it takes another device's
keymap because that device sent the next event, so a key from either the
xrdp session's own keyboard or andris's XTEST device is named by the
keymap it was typed through.

The key 'x' also repaints a small square (SQUARE_SIZE pixels, at SQUARE_X,
SQUARE_Y) in SQUARE_COLOUR, which scripts/e2e-drive.py uses to check that a
small change reaches ryll as a few small updates, not a full-screen one. It
is clear of the centre pixel the frame check reads. The paint is done before
the keysym is printed, so a printed 'x' means the paint request was sent.

The colour is the screenshot oracle in scripts/e2e-drive.py, which imports
COLOUR from here: a centre pixel of that colour proves a frame of this
window reached ryll. The keysym names are the key oracle, and are compared
by name rather than by keycode so the assertion holds under any keycode set
(Xvfb's evdev, or an xfree86 server).

Override-redirect keeps a window manager out of it, and there is none under
Xvfb anyway. With no window manager nobody else moves the focus, so the
focus is set once and kept; it is set again on every Expose in case.

Standard library and python3-xlib only. Output is line-buffered so the shell
script can wait on it.
"""

import argparse
import sys

from Xlib import X
from Xlib import XK
from Xlib import display as xdisplay

# The screenshot oracle, as (R, G, B). Chosen to be unlike anything else:
# not black, white or a primary, and with three distinct channels, so a
# swapped red and blue (BGRX read as RGBX) fails the check rather than
# passing it.
COLOUR = (0x20, 0xA0, 0x60)

# The key that repaints the square, and the square: 64x64 and 64-aligned, so
# it is one damage tile. SQUARE_COLOUR differs from COLOUR in every channel.
SQUARE_KEY = 'x'
SQUARE_X = 128
SQUARE_Y = 128
SQUARE_SIZE = 64
SQUARE_COLOUR = (0xC8, 0x30, 0x90)


def keysym_names():
    """Map keysym values to their names, as XKeysymToString does.

    python3-xlib has no XKeysymToString: its XK.keysym_to_string returns the
    character a Latin-1 keysym types, and None for anything else. Its XK_*
    constants are the keysym table, so invert those, keeping the first name
    defined for a value where several share it, as Xlib does.
    """
    names = {}
    for attr, value in vars(XK).items():
        if attr.startswith('XK_') and isinstance(value, int):
            names.setdefault(value, attr[3:])
    return names


# The core protocol's Shift modifier, and where the XKB group sits in an
# event's state.
SHIFT_MASK = 0x01
GROUP_SHIFT = 13
GROUP_MASK = 0x03


def detail(d, names, event):
    """A KeyPress, as --detail prints it."""
    group = (event.state >> GROUP_SHIFT) & GROUP_MASK
    unshifted = d.keycode_to_keysym(event.detail, 2 * group)
    shifted = d.keycode_to_keysym(event.detail, 2 * group + 1)
    if shifted == X.NoSymbol:
        shifted = unshifted
    name = names.get(unshifted, f'keysym-0x{unshifted:x}')
    shifted_name = names.get(shifted, f'keysym-0x{shifted:x}')
    return f'KEY name={name} shifted={shifted_name} keycode={event.detail} state=0x{event.state:x}'


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--display', required=True, help='the X display, for example :99')
    parser.add_argument('--detail', action='store_true',
                        help='print each key with its keycode, shifted keysym and state, and each MappingNotify')
    args = parser.parse_args()

    d = xdisplay.Display(args.display)
    screen = d.screen()
    root = screen.root
    width = screen.width_in_pixels
    height = screen.height_in_pixels

    colormap = screen.default_colormap
    pixel = colormap.alloc_color(COLOUR[0] * 257, COLOUR[1] * 257, COLOUR[2] * 257).pixel

    square_pixel = colormap.alloc_color(*(c * 257 for c in SQUARE_COLOUR)).pixel

    window = root.create_window(
        0, 0, width, height, 0, screen.root_depth, X.InputOutput, X.CopyFromParent,
        background_pixel=pixel,
        override_redirect=True,
        event_mask=X.ExposureMask | X.KeyPressMask | X.StructureNotifyMask)
    window.set_wm_name('andris-e2e-keylogger')
    window.map()

    square_gc = window.create_gc(foreground=square_pixel)
    names = keysym_names()
    ready = False
    while True:
        event = d.next_event()
        if event.type == X.MapNotify and not ready:
            window.set_input_focus(X.RevertToParent, X.CurrentTime)
            window.clear_area(0, 0, width, height)
            d.sync()
            print('READY', flush=True)
            ready = True
        elif event.type == X.Expose:
            window.set_input_focus(X.RevertToParent, X.CurrentTime)
            d.flush()
        elif event.type == X.MappingNotify:
            d.refresh_keyboard_mapping(event)
            if args.detail:
                print(f'MAPPING request={event.request} first={event.first_keycode} count={event.count}',
                      flush=True)
        elif event.type == X.KeyPress:
            keysym = d.keycode_to_keysym(event.detail, 0)
            name = names.get(keysym, f'keysym-0x{keysym:x}')
            if name == SQUARE_KEY:
                window.fill_rectangle(square_gc, SQUARE_X, SQUARE_Y, SQUARE_SIZE, SQUARE_SIZE)
                d.flush()
            print(detail(d, names, event) if args.detail else name, flush=True)


if __name__ == '__main__':
    sys.exit(main())
