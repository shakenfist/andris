#!/usr/bin/env python3
"""A moving picture for the end-to-end test's video stream checks.

Maps an override-redirect window at --x, --y holding two areas side by side:
a WIDTH x HEIGHT "video" (320x240 by default) at the window's left, repainted
at about --fps frames a second (25 by default) with PutImage, and a static
text area TEXT_WIDTH pixels wide to its right, drawn once. The video is a
picture that looks like a photograph (smooth colour gradients, soft blobs
and a fine texture), panned slowly along a smooth path, with fresh grain each
frame. That is what andris should take for video: the whole area changes
every frame, and lossless compression barely shrinks it, so its draws go as
JPEG. The text area never changes after the first draw, so it must stay on
the lossless draw path. scripts/e2e-drive.py (and through it the xrdp test
bed's scripts/e2e-xrdp-drive.py) starts this, and imports TEXT_WIDTH from
here, so the layout has one definition.

Prints READY once the window is mapped and the first frame is painted. After
--duration seconds, or on SIGUSR1, painting stops and the last frame stays on
screen; the script prints

    STOPPED frames=N seconds=S fps=F cpu=C%

where cpu is this process's user plus system time over the painting period,
as a percentage of one core, and writes the last frame to --last-frame (a
binary PPM) before printing, if one was given, so that a screenshot of the
area can be compared with it exactly. The script then idles, repainting on
Expose, until SIGTERM or SIGINT.

python3-xlib has no BIG-REQUESTS, and a whole frame is over the core
protocol's 256 KiB request limit, so each frame is sent as PutImage strips
under it: two for the default size.

Standard library and python3-xlib only. Output is line-buffered so the shell
script can wait on it.
"""

import argparse
import math
import os
import random
import select
import signal
import sys
import time

from Xlib import X
from Xlib import display as xdisplay

# The static area beside the video, and its text.
TEXT_WIDTH = 240
TEXT_LINES = [
    'andris end-to-end video test',
    '',
    'This text is static: after the',
    'first draw it should never be',
    'damaged, and should stay on the',
    'lossless draw path while the',
    'picture beside it streams.',
]
TEXT_LEFT = 12
TEXT_TOP = 24
TEXT_LEADING = 18
BACKGROUND = (0xE8, 0xE4, 0xD8)
FOREGROUND = (0x20, 0x20, 0x28)

# How far the picture pans, in pixels, along each axis.
PAN = 64

# Grain: the low bits of each colour channel are flipped by fresh random bits
# every frame. Pre-generated, and cycled through.
GRAIN_MASK = 0x07
GRAIN_FRAMES = 16

# The largest PutImage request python3-xlib can send, in bytes, less room
# for the request header.
MAX_REQUEST_BYTES = 65535 * 4 - 64


def source_picture(width, height, seed):
    """A (width + PAN) x (height + PAN) BGRX picture, as bytes, and its stride.

    Smooth gradients, three soft blobs and a fixed fine texture: something
    that looks like a photograph to a compressor. Built once, in pure Python,
    so it is kept small.
    """
    rng = random.Random(seed)
    sw = width + PAN
    sh = height + PAN
    blobs = [(rng.uniform(0, sw), rng.uniform(0, sh), rng.uniform(40, 90), rng.uniform(-70, 70)) for _ in range(3)]
    out = bytearray(sw * sh * 4)
    for y in range(sh):
        for x in range(sw):
            r = 120 + 70 * math.sin(x / 37.0 + y / 91.0) + 30 * math.sin(y / 13.0)
            g = 110 + 60 * math.sin(y / 29.0 - x / 71.0) + 25 * math.cos(x / 17.0)
            b = 130 + 80 * math.cos((x + y) / 53.0)
            for bx, by, radius, gain in blobs:
                d2 = ((x - bx) ** 2 + (y - by) ** 2) / (radius * radius)
                if d2 < 4.0:
                    k = math.exp(-d2)
                    r += gain * k
                    g += 0.6 * gain * k
                    b -= 0.8 * gain * k
            texture = ((x * 7919 + y * 104729) ^ (x * y)) % 23 - 11
            i = (y * sw + x) * 4
            out[i] = max(0, min(255, int(b + texture)))
            out[i + 1] = max(0, min(255, int(g + texture)))
            out[i + 2] = max(0, min(255, int(r + texture)))
    return bytes(out), sw * 4


def grain(width, height, seed):
    """GRAIN_FRAMES masks to XOR over a BGRX frame, as integers."""
    rng = random.Random(seed + 1)
    pixel_mask = bytes([GRAIN_MASK, GRAIN_MASK, GRAIN_MASK, 0]) * (width * height)
    mask_int = int.from_bytes(pixel_mask, 'little')
    return [rng.getrandbits(width * height * 32) & mask_int for _ in range(GRAIN_FRAMES)]


class Picture:
    """Produces the video's frames, as BGRX bytes."""

    def __init__(self, width, height, seed):
        self.width = width
        self.height = height
        self.source, self.stride = source_picture(width, height, seed)
        self.grain = grain(width, height, seed)
        self.length = width * height * 4

    def frame(self, n):
        """Frame n: the source panned along a smooth path, with grain."""
        t = n / 25.0
        dx = int(PAN / 2 + (PAN / 2 - 1) * math.sin(t * 0.9))
        dy = int(PAN / 2 + (PAN / 2 - 1) * math.cos(t * 0.7))
        row = self.width * 4
        first = dy * self.stride + dx * 4
        rows = b''.join(self.source[first + y * self.stride:first + y * self.stride + row]
                        for y in range(self.height))
        grained = int.from_bytes(rows, 'little') ^ self.grain[n % GRAIN_FRAMES]
        return grained.to_bytes(self.length, 'little')


def check_visual(d, screen):
    """Fail unless frames can be sent as 32-bit little-endian BGRX."""
    formats = {f.depth: f.bits_per_pixel for f in d.display.info.pixmap_formats}
    if screen.root_depth != 24 or formats.get(24) != 32 or d.display.info.image_byte_order != X.LSBFirst:
        raise SystemExit(f'need a depth 24, 32 bpp, LSBFirst screen; got depth {screen.root_depth}, '
                         f'formats {formats}, byte order {d.display.info.image_byte_order}')


def put_frame(window, gc, frame, width, height):
    """Send one frame as PutImage strips."""
    row = width * 4
    rows_per_strip = max(1, MAX_REQUEST_BYTES // row)
    for top in range(0, height, rows_per_strip):
        rows = min(rows_per_strip, height - top)
        window.put_image(gc, 0, top, width, rows, X.ZPixmap, 24, 0, frame[top * row:(top + rows) * row])


def draw_text(window, gc, left):
    """Draw the static text area, whose left edge is at left."""
    for i, line in enumerate(TEXT_LINES):
        if line:
            window.draw_text(gc, left + TEXT_LEFT, TEXT_TOP + i * TEXT_LEADING, line.encode())


def write_ppm(path, frame, width, height):
    """Write a BGRX frame as a binary PPM."""
    rgb = bytearray(width * height * 3)
    rgb[0::3] = frame[2::4]
    rgb[1::3] = frame[1::4]
    rgb[2::3] = frame[0::4]
    with open(path, 'wb') as f:
        f.write(f'P6\n{width} {height}\n255\n'.encode())
        f.write(rgb)


def cpu_seconds():
    t = os.times()
    return t.user + t.system


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--display', required=True, help='the X display, for example :99')
    parser.add_argument('--x', type=int, default=0, help='left edge of the window')
    parser.add_argument('--y', type=int, default=0, help='top edge of the window')
    parser.add_argument('--width', type=int, default=320, help='video width, even')
    parser.add_argument('--height', type=int, default=240, help='video height, even')
    parser.add_argument('--fps', type=float, default=25.0, help='frames a second to aim for')
    parser.add_argument('--duration', type=float, default=0.0,
                        help='seconds to paint before stopping; 0 paints until SIGUSR1')
    parser.add_argument('--seed', type=int, default=8, help='picture seed')
    parser.add_argument('--last-frame', help='write the last frame painted here, as a binary PPM')
    args = parser.parse_args()
    if args.width % 2 or args.height % 2:
        raise SystemExit('--width and --height must be even')

    state = {'painting': True, 'running': True}

    def stop_painting(_signum, _frame):
        state['painting'] = False

    def stop(_signum, _frame):
        state['running'] = False

    signal.signal(signal.SIGUSR1, stop_painting)
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    picture = Picture(args.width, args.height, args.seed)

    d = xdisplay.Display(args.display)
    screen = d.screen()
    check_visual(d, screen)
    colormap = screen.default_colormap
    background = colormap.alloc_color(*(c * 257 for c in BACKGROUND)).pixel
    foreground = colormap.alloc_color(*(c * 257 for c in FOREGROUND)).pixel

    window_height = max(args.height, TEXT_TOP + len(TEXT_LINES) * TEXT_LEADING)
    window = screen.root.create_window(
        args.x, args.y, args.width + TEXT_WIDTH, window_height, 0, screen.root_depth, X.InputOutput,
        X.CopyFromParent, background_pixel=background, override_redirect=True,
        event_mask=X.ExposureMask | X.StructureNotifyMask)
    window.set_wm_name('andris-e2e-video')
    gc = window.create_gc(foreground=foreground, background=background)
    window.map()

    while True:
        event = d.next_event()
        if event.type == X.MapNotify:
            break

    frame = picture.frame(0)
    draw_text(window, gc, args.width)
    put_frame(window, gc, frame, args.width, args.height)
    d.sync()
    print('READY', flush=True)

    interval = 1.0 / args.fps
    start = time.monotonic()
    cpu_start = cpu_seconds()
    deadline = start + args.duration if args.duration > 0 else None
    n = 0
    due = start + interval
    stopped_at = None
    while state['running']:
        now = time.monotonic()
        if state['painting'] and deadline is not None and now >= deadline:
            state['painting'] = False
        if not state['painting'] and stopped_at is None:
            stopped_at = now
            seconds = stopped_at - start
            cpu = 100.0 * (cpu_seconds() - cpu_start) / seconds if seconds > 0 else 0.0
            if args.last_frame:
                write_ppm(args.last_frame, frame, args.width, args.height)
            print(f'STOPPED frames={n + 1} seconds={seconds:.2f} fps={n / seconds:.1f} cpu={cpu:.1f}%', flush=True)

        # Wait for the next frame, or an event; a signal also wakes this.
        timeout = max(0.0, due - now) if state['painting'] else 0.5
        select.select([d.fileno()], [], [], timeout)
        while d.pending_events():
            event = d.next_event()
            if event.type == X.Expose and event.count == 0:
                draw_text(window, gc, args.width)
                put_frame(window, gc, frame, args.width, args.height)
                d.flush()
        if state['painting'] and time.monotonic() >= due:
            n += 1
            frame = picture.frame(n)
            put_frame(window, gc, frame, args.width, args.height)
            d.sync()
            # Keep the cadence, but never queue up frames after a stall.
            due = max(due + interval, time.monotonic())
    return 0


if __name__ == '__main__':
    sys.exit(main())
