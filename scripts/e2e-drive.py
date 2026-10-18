#!/usr/bin/env python3
"""Drive ryll's control socket for the end-to-end test.

scripts/e2e.sh runs this once andris, the keylogger and a headless ryll are
up. It asserts, in order:

  surface     within SURFACE_BOUND of starting, ryll's `status` reports a
              connected session with a surface of the expected size;
  frame       an RGBA `screenshot` of that surface has the keylogger's
              colour at its centre pixel. The surface can exist before the
              first draw reaches it, so the screenshot is retried until
              FRAME_BOUND passes; the last pixel seen is reported on
              failure;
  keys        after `send_key` presses of 0x1E and 0xE04B, the keylogger
              prints `a` and then `Left` within KEY_BOUND;
  idle        with nothing changing, no `surface_drawn` event arrives for
              IDLE_WINDOW. andris sends only on X damage, so an idle
              desktop must be silent. To make "nothing changing" true, the
              driver first waits for QUIET seconds without any event (the
              key presses above, and their echo through andris's capture,
              settle), and only then starts the IDLE_WINDOW clock;
  change      `send_key` of 0x2D makes the keylogger repaint a 64x64 square;
              a screenshot shows the new colour there within CHANGE_BOUND,
              and the whole change took fewer `surface_drawn` events than
              the screen has 64-row bands (height / 64). Events are counted
              until QUIET seconds pass without another, so late ones count;
  clipboard   three texts each way, through andris's agent: one inside a
              single agent chunk, one over it, and one over the 1 MiB above
              which X clients send text by INCR. SPICE to X: ryll's
              `clipboard_set`, then `xclip -o` on the display returns the
              text within CLIPBOARD_BOUND, and for the largest, a paste by
              hand with python-xlib shows andris served it by INCR. X to
              SPICE: `xclip -i` on the display, then ryll raises
              `clipboard_changed` with the text's length, and
              `clipboard_get` returns the text;
  resize      ryll's `resize` to a smaller size and back to the start: each
              reaches ryll as a new surface of that size, and andris logs
              each as applied. Then two sizes above the start, which Xvfb's
              RandR does not allow: ryll is told both were refused, andris
              logs the refusal once, and ryll's surface keeps its size;
  video       scripts/e2e-video.py plays a moving picture beside a static
              text area. Polling ryll's `stats` every POLL: within
              STREAM_BOUND one H.264 stream is created and decodes frames,
              and for STREAM_SPELL more it keeps decoding with no decode
              failure, and no second stream starts. Andris logs the
              stream's rectangle, which must cover the video and not the
              text. The video then stops (SIGUSR1): the stream is destroyed
              within DESTROY_BOUND, and within REPAIR_BOUND of that a
              screenshot of the video's area matches its last frame
              exactly, which proves andris repaired the area losslessly.
              Last, a summary of ryll's STREAM_REPORTs and of andris's
              bitrate changes, from andris's debug log, is printed;
  audio       a 440 Hz tone of TONE_SECONDS is played with `pacat` into the
              default sink of the sound server andris captures (the
              environment names it, as PULSE_SERVER). Polling the `playback`
              object of ryll's `stats` every AUDIO_POLL: within
              AUDIO_START_BOUND of the tone starting, one START arrives, for
              Opus at 48 kHz in two channels; no STOP arrives while the tone
              plays, and no second START; once `pacat` has drained and
              exited, a STOP arrives within AUDIO_STOP_BOUND, andris
              stopping after a second of silence. Over the tone ryll must
              have decoded at least AUDIO_PACKETS packets, with no decode
              failure.

With --no-stream, for a ryll started with --no-h264, it checks only that:
once a surface is up, the video plays for NO_STREAM_SPELL with ryll's
`stats` showing no stream created at any poll. scripts/e2e.sh then reads
andris's session-end metrics line for the JPEG draws the video went as.

With --no-audio, for an andris started with --no-audio, it checks only that,
once a frame has arrived, ryll's `stats` have no `playback` object at any
poll for NO_AUDIO_SPELL: ryll was offered no playback channel. scripts/e2e.sh
also checks ryll's log for no playback link.

With --kerbside, for a ryll connected through kerbside's proxy rather than
straight to andris (scripts/e2e-kerbside.sh), it runs only the checks whose
messages cross every channel kerbside relays: surface, frame, keys,
clipboard, video and audio. The idle, small-change and resize checks test
andris rather than the proxy, and are left to the direct run.

Every wait gives up at once if a process named with --watch (andris, ryll,
the keylogger) exits, rather than running out its bound.

The control-socket client follows ryll's tools/proxmox-smoke.py: NDJSON
requests with an integer id, responses matched by id, events skipped. See
ryll's docs/control-socket-protocol.md. `send_key` takes the logical set-1
make code, with the 0xE0 prefix in the high byte. Protocol 1.4 is required,
for the `stats` verb; 1.3 added the clipboard and resize verbs, and 1.2
fixed extended keys, which older servers sent with the bytes the wrong way
round. The audio checks need 1.6, which added `stats`' `playback` object.

The resize checks read andris's log (--andris-log), and ryll's (--ryll-log),
whose debug lines record each agent reply andris sends.

Exit status: 0 when every check passed, 1 otherwise.
"""

import argparse
import array
import base64
import importlib.util
import json
import math
import os
import re
import select
import signal
import socket
import subprocess
import sys
import time

from Xlib import X
from Xlib import display as xdisplay

PREFIX = '[e2e-drive]'

# From starting: the control socket must accept a connection, and status
# must show a surface, within SURFACE_BOUND.
SURFACE_BOUND = 30.0

# From the first surface: a screenshot must show the keylogger's colour.
FRAME_BOUND = 10.0

# From the key presses: the keylogger must have printed both keysyms.
KEY_BOUND = 5.0

# Idle check: how long to demand silence, and how long without an event
# counts as settled. QUIET is longer than andris's frame interval by a wide
# margin; QUIET_BOUND stops a never-quiet desktop from hanging the test.
IDLE_WINDOW = 2.0
QUIET = 0.75
QUIET_BOUND = 15.0

# The small change: from the key press, a screenshot must show the colour.
CHANGE_BOUND = 2.0

POLL = 0.25

# The control protocol needed: 1.4 added `stats`, which the video checks
# read streams from.
PROTOCOL = (1, 4)

# Clipboard: from one side's copy to the other side having the text. Ryll
# looks at its clipboard every 500 ms, and the largest text crosses the
# agent in about 800 chunks, so this is generous.
CLIPBOARD_BOUND = 15.0

# The largest agent chunk, and the size above which andris (and xclip) send
# text by INCR rather than in one property.
AGENT_CHUNK = 2048
INCR_THRESHOLD = 1024 * 1024

# Resize: from ryll's request to its new surface, or to the reply that
# refused it. Ryll debounces sizes for 200 ms.
RESIZE_BOUND = 10.0

# ryll queues 256 events per client, then sends `dropped`; counts are unsound
# after that, so it is a failure.
BAND = 64

# The keys sent, as (logical set-1 make code, the keysym name the keylogger
# must print).
KEYS = ((0x1E, 'a'), (0xE04B, 'Left'))

# The key that makes the keylogger repaint its square: set-1 'x'.
SQUARE_SCANCODE = 0x2D

# The video's window, whose left edge and top are 64-aligned, so that the
# tiles the video changes end where the text area's begin: andris's stream
# is the bounding box of whole tiles.
VIDEO_X = 64
VIDEO_Y = 64

# From the video's first frame to an H.264 stream in ryll that has decoded
# frames: andris wants 20 frames of change first, under a second at 25 fps.
STREAM_BOUND = 15.0
# How long the stream is then watched decoding.
STREAM_SPELL = 3.0
# From SIGUSR1 to the video, to ryll seeing the stream destroyed: andris
# ends a stream a second after its last change.
DESTROY_BOUND = 2.0
# From the destroy to a screenshot matching the video's last frame: the
# lossless refresh of the stream's area is due at once, behind any frame.
REPAIR_BOUND = 2.0
# The --no-stream run: how long the video plays with no stream allowed.
NO_STREAM_SPELL = 5.0
# How long the video script may take to build its picture and map.
VIDEO_BOUND = 30.0

# The control protocol the audio checks need: 1.6 added `stats`' playback
# object.
AUDIO_PROTOCOL = (1, 6)
# The tone: a 440 Hz sine at a quarter of full scale, the same in both
# channels, as raw S16LE stereo at 48 kHz, the format andris captures.
TONE_FREQUENCY = 440.0
TONE_AMPLITUDE = 8192
TONE_RATE = 48000
TONE_SECONDS = 5.0
# From the start of the audio check to ryll's stats having a playback
# object, which ryll publishes every 100 ms while the channel is silent.
PLAYBACK_BOUND = 10.0
# From `pacat` starting to ryll seeing a START. Andris starts on the first
# frame over its silence threshold; a PulseAudio null sink can take a
# second or two to wake.
AUDIO_START_BOUND = 5.0
# From `pacat` exiting, once its stream has drained, to ryll seeing a STOP:
# andris stops after a second of silence.
AUDIO_STOP_BOUND = 2.0
# How long `pacat` may take beyond the tone itself, to connect and drain.
TONE_BOUND = TONE_SECONDS + 10.0
# The packets ryll must decode over the tone, which is 500 10 ms frames.
AUDIO_PACKETS = 400
AUDIO_POLL = 0.1
# The --no-audio run: how long ryll's stats must show no playback object.
NO_AUDIO_SPELL = 3.0


def log(message):
    print(f'{PREFIX} {message}', flush=True)


def keylogger_module():
    """Import the keylogger, so the oracles have one definition."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'e2e-keylogger.py')
    spec = importlib.util.spec_from_file_location('e2e_keylogger', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Control:
    """A minimal client for ryll's control socket (see the module docstring)."""

    def __init__(self, path, timeout=10.0):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.buf = bytearray()
        self.next_id = 0
        # The control protocol ryll speaks, as a tuple; connect() sets it.
        self.protocol = None
        # Events seen while waiting for a response, as (monotonic time, name,
        # data).
        self.events = []

    def close(self):
        self.sock.close()

    def _recv_line(self):
        while b'\n' not in self.buf:
            chunk = self.sock.recv(1 << 20)
            if not chunk:
                raise EOFError('ryll closed the control socket')
            self.buf.extend(chunk)
        nl = self.buf.index(b'\n')
        line = bytes(self.buf[:nl])
        del self.buf[:nl + 1]
        return json.loads(line.decode('utf-8'))

    def request(self, method, params=None):
        self.next_id += 1
        req_id = self.next_id
        line = json.dumps({'id': req_id, 'method': method, 'params': params or {}}) + '\n'
        self.sock.sendall(line.encode('utf-8'))
        while True:
            msg = self._recv_line()
            if msg.get('id') == req_id:
                return msg
            self._note(msg)

    def _note(self, msg):
        if 'event' in msg:
            if msg['event'] == 'dropped':
                raise Failure(f'ryll dropped control events: {msg}')
            self.events.append((time.monotonic(), msg['event'], msg.get('data') or {}))

    def pump(self, seconds):
        """Read events for up to `seconds`, leaving them in self.events."""
        deadline = time.monotonic() + seconds
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            try:
                self.sock.settimeout(remaining)
                msg = self._recv_line()
            except socket.timeout:
                return
            finally:
                self.sock.settimeout(10.0)
            self._note(msg)

    def wait_event(self, name, test, bound, watch, start=0):
        """Wait up to `bound` for event `name` whose data passes test(data), looking from self.events[start]
        onwards; return its data."""
        deadline = time.monotonic() + bound
        while True:
            for _, seen, data in self.events[start:]:
                if seen == name and test(data):
                    return data
            start = len(self.events)
            watch.check()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise Failure(f'no {name} event within {bound:.0f} s')
            self.pump(min(remaining, POLL))

    def drawn(self):
        return sum(1 for _, name, _ in self.events if name == 'surface_drawn')

    def call(self, method, params=None):
        """Return a request's result, or raise if ryll refused it."""
        resp = self.request(method, params)
        if not resp.get('ok'):
            raise RuntimeError(f'{method} refused: {resp.get("error")}')
        return resp['result']


class Failure(Exception):
    pass


class Watch:
    """Processes that must stay up; a wait gives up as soon as one exits."""

    def __init__(self, specs):
        self.pids = {}
        for spec in specs:
            name, _, pid = spec.partition('=')
            self.pids[name] = int(pid)

    def check(self):
        for name, pid in self.pids.items():
            try:
                with open(f'/proc/{pid}/stat', encoding='utf-8') as f:
                    state = f.read().rsplit(')', 1)[1].split()[0]
            except (OSError, IndexError):
                state = None
            if state in (None, 'Z', 'X'):
                raise Failure(f'{name} (pid {pid}) exited')


def sleep(watch):
    watch.check()
    time.sleep(POLL)


def connect(path, deadline, watch):
    """Connect and say hello, retrying until the socket appears or deadline."""
    last = None
    while time.monotonic() < deadline:
        try:
            control = Control(path)
        except OSError as e:
            last = e
            sleep(watch)
            continue
        wanted = '.'.join(str(part) for part in PROTOCOL)
        hello = control.call('hello', {'client_name': 'andris-e2e', 'protocol_version': wanted})
        version = tuple(int(part) for part in hello['protocol_version'].split('.'))
        if version < PROTOCOL:
            raise Failure(f'ryll speaks control protocol {hello["protocol_version"]}; {wanted} or later is '
                          'needed for the stats verb')
        log(f'connected to {path}, protocol {hello["protocol_version"]}')
        control.protocol = version
        return control
    raise Failure(f'could not connect to {path} within {SURFACE_BOUND:.0f} s: {last}')


def wait_surface(control, deadline, width, height, watch):
    status = None
    while time.monotonic() < deadline:
        status = control.call('status')
        if status.get('spice_connected') and status.get('surfaces'):
            surface = status['surfaces'][0]
            log(f'surface: {surface}')
            if (surface['width'], surface['height']) != (width, height):
                raise Failure(f'surface is {surface["width"]}x{surface["height"]}, expected {width}x{height}')
            return surface
        sleep(watch)
    raise Failure(f'no surface within {SURFACE_BOUND:.0f} s; last status: {status}')


def centre_pixel(control, surface_id):
    result = control.call('screenshot', {'surface_id': surface_id, 'format': 'rgba'})
    width = result['width']
    height = result['height']
    data = base64.b64decode(result['data_base64'])
    if len(data) != width * height * 4:
        raise Failure(f'rgba screenshot is {len(data)} bytes, expected {width * height * 4} for {width}x{height}')
    offset = ((height // 2) * width + width // 2) * 4
    return tuple(data[offset:offset + 3])


def wait_frame(control, surface_id, colour, watch):
    deadline = time.monotonic() + FRAME_BOUND
    pixel = None
    while time.monotonic() < deadline:
        pixel = centre_pixel(control, surface_id)
        if pixel == colour:
            log(f'centre pixel is #{bytes(pixel).hex()}, the keylogger\'s colour')
            return
        sleep(watch)
    expected = bytes(colour).hex()
    seen = bytes(pixel).hex() if pixel else 'nothing'
    raise Failure(f'centre pixel did not become #{expected} within {FRAME_BOUND:.0f} s; last seen #{seen}')


def keylog_lines(path):
    """The keysym names the keylogger printed after READY."""
    with open(path, encoding='utf-8', errors='replace') as f:
        lines = [line.strip() for line in f]
    if 'READY' not in lines:
        raise Failure(f'the keylogger never printed READY in {path}')
    return lines[lines.index('READY') + 1:]


def is_subsequence(wanted, seen):
    it = iter(seen)
    return all(any(name == item for item in it) for name in wanted)


def send_keys(control, keylog, watch):
    for scancode, _ in KEYS:
        control.call('send_key', {'scancode': scancode, 'state': 'press'})
        log(f'sent 0x{scancode:X}')
    wanted = [name for _, name in KEYS]
    deadline = time.monotonic() + KEY_BOUND
    seen = []
    while time.monotonic() < deadline:
        seen = keylog_lines(keylog)
        if is_subsequence(wanted, seen):
            log(f'keylogger printed {seen}')
            return
        sleep(watch)
    raise Failure(f'keylogger did not print {wanted} in order within {KEY_BOUND:.0f} s; it printed {seen}')


def wait_quiet(control, watch):
    """Return once QUIET seconds have passed with no event at all."""
    deadline = time.monotonic() + QUIET_BOUND
    while time.monotonic() < deadline:
        control.events.clear()
        control.pump(QUIET)
        watch.check()
        if not control.events:
            return
    raise Failure(f'events never stopped for {QUIET} s within {QUIET_BOUND:.0f} s')


def check_idle(control, watch):
    wait_quiet(control, watch)
    control.events.clear()
    control.pump(IDLE_WINDOW)
    watch.check()
    if control.events:
        names = sorted({name for _, name, _ in control.events})
        raise Failure(f'{len(control.events)} events ({", ".join(names)}) arrived in {IDLE_WINDOW:.0f} s of idle')
    log(f'idle: no events for {IDLE_WINDOW:.0f} s')


def pixel_at(control, surface_id, x, y):
    result = control.call('screenshot', {'surface_id': surface_id, 'format': 'rgba'})
    width = result['width']
    data = base64.b64decode(result['data_base64'])
    offset = (y * width + x) * 4
    return tuple(data[offset:offset + 3])


def check_small_change(control, surface_id, keylogger, height, watch):
    centre = keylogger.SQUARE_SIZE // 2
    x = keylogger.SQUARE_X + centre
    y = keylogger.SQUARE_Y + centre
    wanted = keylogger.SQUARE_COLOUR
    if pixel_at(control, surface_id, x, y) == wanted:
        raise Failure('the square already has its new colour before the key was sent')
    control.events.clear()
    start = time.monotonic()
    control.call('send_key', {'scancode': SQUARE_SCANCODE, 'state': 'press'})
    pixel = None
    while time.monotonic() - start < CHANGE_BOUND:
        pixel = pixel_at(control, surface_id, x, y)
        if pixel == wanted:
            break
        watch.check()
        control.pump(0.05)
    else:
        raise Failure(f'pixel ({x},{y}) did not become #{bytes(wanted).hex()} within {CHANGE_BOUND:.0f} s; '
                      f'last seen #{bytes(pixel).hex()}')
    log(f'square changed after {time.monotonic() - start:.2f} s')
    wait_quiet_counting(control, watch)
    bands = height // BAND
    count = control.drawn()
    if count == 0 or count >= bands:
        raise Failure(f'the small change took {count} surface_drawn events; expected 1 to {bands - 1}')
    log(f'small change: {count} surface_drawn events (fewer than {bands} bands)')


def wait_quiet_counting(control, watch):
    """Like wait_quiet, but keep the events seen on the way."""
    deadline = time.monotonic() + QUIET_BOUND
    while time.monotonic() < deadline:
        mark = len(control.events)
        control.pump(QUIET)
        watch.check()
        if len(control.events) == mark:
            return
    raise Failure(f'events never stopped for {QUIET} s within {QUIET_BOUND:.0f} s')


# -- logs


# A colour escape, in case a log has them.
ESCAPE = re.compile(r'\x1b\[[0-9;]*m')


def log_lines(path, pattern):
    """The lines of the log at `path` that match regex `pattern`, without colour escapes."""
    with open(path, encoding='utf-8', errors='replace') as f:
        lines = [ESCAPE.sub('', line).rstrip('\n') for line in f]
    return [line for line in lines if re.search(pattern, line)]


# -- the clipboard


def clipboard_text(label, size):
    """At least `size` bytes of UTF-8 text, all of it unique to `label` and this run, with multi-byte characters
    on every line, so that a copy that crossed before, or a truncated or mangled one, cannot pass."""
    lines = [f'andris e2e {label} {os.getpid()} {time.time_ns()}\n']
    length = len(lines[0].encode('utf-8'))
    number = 0
    while length < size:
        line = f'{label} line {number:06d}: héllo wörld, 世界 🎉 — the quick brown fox\n'
        lines.append(line)
        length += len(line.encode('utf-8'))
        number += 1
    return ''.join(lines)


def xclip_copy(display, data):
    """Copy `data` to CLIPBOARD on `display` with xclip, which serves it until another program takes the
    selection, then exits. -quiet keeps it in the foreground, so the caller can kill it."""
    child = subprocess.Popen(['xclip', '-display', display, '-selection', 'clipboard', '-i', '-quiet'],
                             stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    # xclip takes the selection once its input is closed.
    child.stdin.write(data)
    child.stdin.close()
    return child


def xclip_paste(display):
    """CLIPBOARD on `display` as UTF8_STRING, by xclip, or None if there is no owner or it refuses."""
    try:
        result = subprocess.run(['xclip', '-display', display, '-selection', 'clipboard', '-o', '-t', 'UTF8_STRING'],
                                capture_output=True, timeout=10)
    except subprocess.TimeoutExpired:
        return None
    return result.stdout if result.returncode == 0 else None


def stop(child):
    if child.poll() is None:
        child.kill()
    child.wait()


def wait_paste(display, data, bound, watch):
    """Paste CLIPBOARD on `display` until it is `data`; return the seconds that took."""
    start = time.monotonic()
    pasted = None
    while time.monotonic() - start < bound:
        pasted = xclip_paste(display)
        if pasted == data:
            return time.monotonic() - start
        watch.check()
        time.sleep(POLL / 5)
    seen = 'nothing' if pasted is None else f'{len(pasted)} other bytes'
    raise Failure(f'pasting on {display} did not give the {len(data)} bytes within {bound:.0f} s; last got {seen}')


def spice_to_x(control, display, text, watch, bound=CLIPBOARD_BOUND):
    """Set ryll's clipboard to `text`, and wait for a paste on `display` to give it; return the seconds taken."""
    control.call('clipboard_set', {'text': text})
    return wait_paste(display, text.encode('utf-8'), bound, watch)


def wait_clipboard_changed(control, text, mark, watch, bound=CLIPBOARD_BOUND):
    """Wait for ryll to report a copy of `text`'s length from control.events[mark] on, and for `clipboard_get`
    to return `text`."""
    size = len(text.encode('utf-8'))
    control.wait_event('clipboard_changed', lambda data: data.get('bytes') == size, bound, watch, start=mark)
    got = control.call('clipboard_get').get('text')
    if got != text:
        seen = 'null' if got is None else f'{len(got.encode("utf-8"))} other bytes'
        raise Failure(f'ryll reported a copy of {size} bytes, but clipboard_get returned {seen}')


def x_to_spice(control, display, text, watch, bound=CLIPBOARD_BOUND):
    """Copy `text` on `display` with xclip, and wait for ryll to have it; return the seconds taken and the
    xclip process, which the caller stops."""
    mark = len(control.events)
    start = time.monotonic()
    copier = xclip_copy(display, text.encode('utf-8'))
    try:
        wait_clipboard_changed(control, text, mark, watch, bound)
    except BaseException:
        stop(copier)
        raise
    return time.monotonic() - start, copier


def paste_by_hand(name, bound=CLIPBOARD_BOUND):
    """Paste CLIPBOARD on display `name` as UTF8_STRING with python-xlib, following INCR. Return the bytes and
    the number of INCR pieces, or 0 if the owner sent the text in one property."""
    d = xdisplay.Display(name)
    try:
        screen = d.screen()
        window = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth, event_mask=X.PropertyChangeMask)
        clipboard = d.intern_atom('CLIPBOARD')
        utf8 = d.intern_atom('UTF8_STRING')
        incr = d.intern_atom('INCR')
        prop = d.intern_atom('ANDRIS_E2E_PASTE')
        deadline = time.monotonic() + bound

        def next_event(test):
            while True:
                while d.pending_events():
                    event = d.next_event()
                    if test(event):
                        return event
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise Failure(f'pasting by hand on {name} took over {bound:.0f} s')
                select.select([d.fileno()], [], [], remaining)

        window.convert_selection(clipboard, utf8, prop, X.CurrentTime)
        d.flush()
        notify = next_event(lambda e: e.type == X.SelectionNotify)
        if notify.property == X.NONE:
            raise Failure(f'the owner of CLIPBOARD on {name} refused a paste as UTF8_STRING')
        value = window.get_full_property(prop, X.AnyPropertyType)
        window.delete_property(prop)
        d.flush()
        if value is None or value.property_type != incr:
            return (bytes(value.value) if value else b''), 0
        pieces = []
        while True:
            next_event(lambda e: e.type == X.PropertyNotify and e.atom == prop and e.state == X.PropertyNewValue)
            value = window.get_full_property(prop, X.AnyPropertyType)
            window.delete_property(prop)
            d.flush()
            piece = bytes(value.value) if value else b''
            if not piece:
                return b''.join(pieces), len(pieces)
            pieces.append(piece)
    finally:
        d.close()


def check_clipboard(control, display, watch):
    """Text inside one agent chunk, over one, and over the INCR threshold, each way."""
    sizes = (('small', 100), ('chunked', 3 * AGENT_CHUNK), ('incr', INCR_THRESHOLD + INCR_THRESHOLD // 2))
    copiers = []
    try:
        for label, size in sizes:
            text = clipboard_text(f'spice-to-x-{label}', size)
            length = len(text.encode('utf-8'))
            elapsed = spice_to_x(control, display, text, watch)
            note = ''
            if length > INCR_THRESHOLD:
                data, pieces = paste_by_hand(display)
                if data != text.encode('utf-8'):
                    raise Failure(f'a paste by hand of the {length}-byte text gave {len(data)} other bytes')
                if not pieces:
                    raise Failure(f'andris served the {length}-byte text in one property, not by INCR')
                note = f', by INCR in {pieces} pieces'
            log(f'clipboard: SPICE to X, {length} bytes ({label}) pasted after {elapsed:.2f} s{note}')

            text = clipboard_text(f'x-to-spice-{label}', size)
            elapsed, copier = x_to_spice(control, display, text, watch)
            copiers.append(copier)
            log(f'clipboard: X to SPICE, {len(text.encode("utf-8"))} bytes ({label}) reached ryll after '
                f'{elapsed:.2f} s')
    finally:
        for copier in copiers:
            stop(copier)


# -- resize


def wait_surface_size(control, size, watch, bound=RESIZE_BOUND):
    """Wait for ryll's primary surface to be `size`; return the seconds that took."""
    start = time.monotonic()
    surfaces = None
    while time.monotonic() - start < bound:
        surfaces = control.call('status').get('surfaces') or []
        if surfaces and (surfaces[0]['width'], surfaces[0]['height']) == size:
            return time.monotonic() - start
        sleep(watch)
    raise Failure(f'ryll has no {size[0]}x{size[1]} surface within {bound:.0f} s; it has {surfaces}')


def surface_size(control):
    surfaces = control.call('status').get('surfaces') or []
    if not surfaces:
        raise Failure('ryll has no surface')
    return surfaces[0]['width'], surfaces[0]['height']


# Ryll's debug line for each agent REPLY, to a monitors config (type 2);
# error 0 is success.
REPLY = re.compile(r'main: VD_AGENT_REPLY type=2 error=(\d+)')

# Andris's lines for a resize applied, and refused (logged once per
# connection); see andris/src/server/agent.rs.
APPLIED = 'resized the desktop to {size}, as the client asked'
REFUSED = r'the client asked for a \d+x\d+ desktop, and the desktop refused: .* \(logged once per connection\)'


def refusal_reason(line):
    """Why andris's refusal line says the desktop refused."""
    return line.split('refused: ', 1)[1].split('; it keeps its size', 1)[0]


def refusals(ryll_log):
    return sum(1 for line in log_lines(ryll_log, REPLY.pattern) if REPLY.search(line).group(1) != '0')


def request_size(control, size):
    """Ask for `size` with ryll's `resize`, retrying while its queue is full."""
    deadline = time.monotonic() + RESIZE_BOUND
    while True:
        resp = control.request('resize', {'width': size[0], 'height': size[1]})
        if resp.get('ok'):
            result = resp['result']
            if (result['width'], result['height']) != size:
                raise Failure(f'ryll asked for {result["width"]}x{result["height"]}, not {size[0]}x{size[1]}')
            return
        if resp.get('error', {}).get('code') != 'queue_full' or time.monotonic() >= deadline:
            raise RuntimeError(f'resize refused: {resp.get("error")}')
        time.sleep(POLL)


def wait_refused(ryll_log, before, watch):
    """Wait for ryll's log to show a refused resize beyond `before` of them; return the count."""
    deadline = time.monotonic() + RESIZE_BOUND
    while True:
        count = refusals(ryll_log)
        if count > before:
            return count
        watch.check()
        if time.monotonic() >= deadline:
            raise Failure(f'ryll was not told of a refused resize within {RESIZE_BOUND:.0f} s')
        time.sleep(POLL)


def check_resize(control, start_size, andris_log, ryll_log, watch):
    """A shrink and a grow back are applied; two sizes above the start are refused, logged once."""
    smaller = (start_size[0] * 3 // 4 // 8 * 8, start_size[1] * 3 // 4 // 8 * 8)
    for size in (smaller, start_size):
        request_size(control, size)
        elapsed = wait_surface_size(control, size, watch)
        text = f'{size[0]}x{size[1]}'
        if not log_lines(andris_log, re.escape(APPLIED.format(size=text))):
            raise Failure(f'ryll has a {text} surface, but andris never logged "{APPLIED.format(size=text)}"')
        log(f'resize: {text} applied, and reached ryll as a new surface after {elapsed:.2f} s')

    above = ((start_size[0] + 256, start_size[1] + 256), (start_size[0] + 512, start_size[1] + 512))
    before = refusals(ryll_log)
    for size in above:
        request_size(control, size)
        before = wait_refused(ryll_log, before, watch)
        now = surface_size(control)
        if now != start_size:
            raise Failure(f'after a refused resize to {size[0]}x{size[1]}, ryll\'s surface is {now[0]}x{now[1]}, '
                          f'not {start_size[0]}x{start_size[1]}')
    lines = log_lines(andris_log, REFUSED)
    if len(lines) != 1:
        raise Failure(f'andris logged {len(lines)} refusals for {len(above)} refused resizes, not 1: {lines}')
    # The refusals' replies came back before the surface was read, so a
    # late change would be a new surface after them; give it a moment.
    control.pump(QUIET)
    now = surface_size(control)
    if now != start_size:
        raise Failure(f'after the refused resizes ryll\'s surface became {now[0]}x{now[1]}')
    log(f'resize: {" and ".join(f"{w}x{h}" for w, h in above)} refused, logged once '
        f'({refusal_reason(lines[0])}); the surface stayed {start_size[0]}x{start_size[1]}')


# -- the video


def video_module():
    """Import the video script, for its layout."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'e2e-video.py')
    spec = importlib.util.spec_from_file_location('e2e_video', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def overlaps(a, b):
    """Whether two (x, y, width, height) rectangles share a pixel."""
    return a[0] < b[0] + b[2] and b[0] < a[0] + a[2] and a[1] < b[1] + b[3] and b[1] < a[1] + a[3]


def describe_rect(rect):
    return f'{rect[2]}x{rect[3]}+{rect[0]}+{rect[1]}'


class Video:
    """scripts/e2e-video.py, playing on a display, its log and its last frame in `work`."""

    def __init__(self, display, work, name, watch):
        module = video_module()
        self.width = 320
        self.height = 240
        self.rect = (VIDEO_X, VIDEO_Y, self.width, self.height)
        self.text = (VIDEO_X + self.width, VIDEO_Y, module.TEXT_WIDTH, self.height)
        self.log_path = os.path.join(work, f'{name}.log')
        self.frame_path = os.path.join(work, f'{name}-last.ppm')
        script = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'e2e-video.py')
        with open(self.log_path, 'wb') as out:
            self.child = subprocess.Popen(
                [sys.executable, '-u', script, '--display', display, '--x', str(VIDEO_X), '--y', str(VIDEO_Y),
                 '--width', str(self.width), '--height', str(self.height), '--last-frame', self.frame_path],
                stdout=out, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        self.wait_line('READY', VIDEO_BOUND, watch)

    def lines(self):
        with open(self.log_path, encoding='utf-8', errors='replace') as f:
            return [line.strip() for line in f]

    def wait_line(self, prefix, bound, watch):
        deadline = time.monotonic() + bound
        while True:
            found = [line for line in self.lines() if line.startswith(prefix)]
            if found:
                return found[0]
            if self.child.poll() is not None:
                raise Failure(f'the video script exited {self.child.returncode}: {self.lines()[-5:]}')
            watch.check()
            if time.monotonic() >= deadline:
                raise Failure(f'the video script did not print {prefix} within {bound:.0f} s: {self.lines()[-5:]}')
            time.sleep(POLL / 5)

    def stop(self, watch):
        """Stop painting, leaving the last frame on screen; return the STOPPED line."""
        self.child.send_signal(signal.SIGUSR1)
        return self.wait_line('STOPPED', 10.0, watch)

    def close(self):
        if self.child.poll() is None:
            self.child.terminate()
            try:
                self.child.wait(10)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait()

    def last_frame(self):
        """The last frame painted, from its PPM, as RGB bytes."""
        with open(self.frame_path, 'rb') as f:
            data = f.read()
        header = f'P6\n{self.width} {self.height}\n255\n'.encode()
        if not data.startswith(header) or len(data) != len(header) + self.width * self.height * 3:
            raise Failure(f'{self.frame_path} is not a {self.width}x{self.height} binary PPM')
        return data[len(header):]


def display_stats(control):
    return control.call('stats')['display']


def decode_failures(display):
    """Every decode failure ryll's stats show, as text; empty if none."""
    found = []
    for codec, counts in (display.get('decodes') or {}).items():
        if counts.get('decode_failures_total'):
            found.append(f'{codec}: {counts["decode_failures_total"]} failed decodes')
    for stream in (display.get('streams') or []) + (display.get('streams_recently_destroyed') or []):
        if stream.get('frames_decode_failed'):
            found.append(f'stream {stream["stream_id"]}: {stream["frames_decode_failed"]} failed frames')
    return found


def poll_stats(control, what, bound, test, watch):
    """Poll `stats` every POLL until test(display) is true, failing on any decode failure; return the display
    stats that passed."""
    deadline = time.monotonic() + bound
    display = None
    while True:
        display = display_stats(control)
        failures = decode_failures(display)
        if failures:
            raise Failure(f'ryll failed to decode stream frames ({"; ".join(failures)}); stats: {display}')
        if test(display):
            return display
        watch.check()
        if time.monotonic() >= deadline:
            raise Failure(f'{what} within {bound:.0f} s; last stats: {display}')
        time.sleep(POLL)


def h264_decoding(display):
    return [s for s in display.get('streams') or [] if s['codec'] == 'h264' and s['frames_decoded_ok'] > 0]


# Andris's info line for a stream starting, with its rectangle as
# WxH+X+Y; see andris/src/server/display.rs.
STARTED = re.compile(r'starting a video stream .*\brect=(\d+)x(\d+)\+(\d+)\+(\d+)')


def stream_rects(andris_log):
    rects = []
    for line in log_lines(andris_log, STARTED.pattern):
        w, h, x, y = (int(part) for part in STARTED.search(line).groups())
        rects.append((x, y, w, h))
    return rects


def screenshot_rgb(control, surface_id, rect):
    """The RGB bytes of `rect` of a screenshot of the surface."""
    result = control.call('screenshot', {'surface_id': surface_id, 'format': 'rgba'})
    width = result['width']
    data = base64.b64decode(result['data_base64'])
    x, y, w, h = rect
    out = bytearray()
    for row in range(y, y + h):
        start = (row * width + x) * 4
        rgba = data[start:start + w * 4]
        out += bytes(b for i, b in enumerate(rgba) if i % 4 != 3)
    return bytes(out)


def differing(a, b):
    """How many pixels of two RGB buffers differ, and by how much at most in one channel."""
    pixels = sum(1 for i in range(0, len(a), 3) if a[i:i + 3] != b[i:i + 3])
    worst = max((abs(p - q) for p, q in zip(a, b)), default=0)
    return pixels, worst


def check_video(control, surface_id, display, work, andris_log, watch):
    """An H.264 stream of the video, its end, and the lossless repair of its area."""
    video = Video(display, work, 'video', watch)
    try:
        start = time.monotonic()
        decoding = poll_stats(control, 'no H.264 stream decoded frames', STREAM_BOUND,
                              lambda d: h264_decoding(d), watch)
        stream = h264_decoding(decoding)[0]
        log(f'video: an H.264 stream ({stream["width"]}x{stream["height"]}) was decoding frames after '
            f'{time.monotonic() - start:.1f} s')
        first = stream['frames_decoded_ok']
        time.sleep(STREAM_SPELL)
        later = poll_stats(control, 'stats could not be read', 1.0, lambda d: True, watch)
        if later['streams_created_total'] != 1:
            raise Failure(f'{later["streams_created_total"]} streams were created for one video; stats: {later}')
        live = [s for s in later['streams'] if s['stream_id'] == stream['stream_id']]
        if not live or live[0]['frames_decoded_ok'] <= first:
            raise Failure(f'the stream stopped decoding frames within {STREAM_SPELL:.0f} s; stats: {later}')
        log(f'video: one stream created, {live[0]["frames_decoded_ok"]} frames decoded, no decode failures, '
            f'{later["stream_reports_sent_total"]} stream reports sent')

        rects = stream_rects(andris_log)
        if len(rects) != 1:
            raise Failure(f'andris logged {len(rects)} streams starting, not 1: {rects}')
        if not overlaps(rects[0], video.rect) or overlaps(rects[0], video.text):
            raise Failure(f'andris streamed {describe_rect(rects[0])}, which does not cover the video at '
                          f'{describe_rect(video.rect)} alone: the text area is at {describe_rect(video.text)}')
        log(f'video: andris streamed {describe_rect(rects[0])}, clear of the text area at '
            f'{describe_rect(video.text)}')

        stopped = time.monotonic()
        log(f'video: {video.stop(watch)}')
        destroyed = poll_stats(control, 'the stream was not destroyed', DESTROY_BOUND,
                               lambda d: d['streams_destroyed_total'] >= 1 and not d['streams'], watch)
        took = time.monotonic() - stopped
        log(f'video: the stream was destroyed {took:.2f} s after the video stopped '
            f'({destroyed["streams_destroyed_total"]} destroyed in all)')

        want = video.last_frame()
        repaired = time.monotonic()
        got = b''
        while True:
            got = screenshot_rgb(control, surface_id, video.rect)
            if got == want:
                break
            watch.check()
            if time.monotonic() - repaired >= REPAIR_BOUND:
                pixels, worst = differing(got, want)
                raise Failure(f'{REPAIR_BOUND:.0f} s after the stream ended, {pixels} pixels of the video\'s area '
                              f'still differ from its last frame, by up to {worst} in a channel')
            time.sleep(POLL / 5)
        log(f'video: {time.monotonic() - repaired:.2f} s after the destroy, a screenshot of the video\'s area '
            f'matched its last frame exactly')
    finally:
        video.close()


# Andris's debug lines for a client's stream report, and a bitrate change.
REPORT = re.compile(r'a stream report .*\bframes=(\d+) drops=(\d+) last_frame_delay_ms=(-?\d+)')
CHANGED = re.compile(r"the stream's bitrate changed .*\bfrom_kbps=(\d+) to_kbps=(\d+) reason=(\w+)")


def report_summary(andris_log):
    """What ryll's STREAM_REPORTs said, and what andris's rate control made of them, from andris's log."""
    reports = [tuple(int(v) for v in REPORT.search(line).groups()) for line in log_lines(andris_log, REPORT.pattern)]
    changes = [CHANGED.search(line).groups() for line in log_lines(andris_log, CHANGED.pattern)]
    if not reports:
        return 'andris logged no stream reports (is andris::server::display at debug?)'
    delays = sorted(delay for _, _, delay in reports)
    drops = sum(drop for _, drop, _ in reports)
    moved = ', '.join(f'{a}->{b} kbit/s ({why})' for a, b, why in changes) or 'none'
    return (f'{len(reports)} stream reports covering {sum(f for f, _, _ in reports)} frames, {drops} late; '
            f'last_frame_delay min {delays[0]} ms, median {delays[len(delays) // 2]} ms, max {delays[-1]} ms; '
            f'bitrate changes: {moved}')


def check_no_stream(control, display, work, watch):
    """With a ryll that has no CODEC_H264, the video never becomes a stream."""
    video = Video(display, work, 'video-no-h264', watch)
    try:
        deadline = time.monotonic() + NO_STREAM_SPELL
        polls = 0
        while time.monotonic() < deadline:
            stats = display_stats(control)
            polls += 1
            if stats['streams_created_total'] or stats['streams']:
                raise Failure(f'a stream was created for a client without CODEC_H264; stats: {stats}')
            watch.check()
            time.sleep(POLL)
        log(f'no-h264: the video played for {NO_STREAM_SPELL:.0f} s, and {polls} polls of stats showed no stream')
        log(f'no-h264: {video.stop(watch)}')
    finally:
        video.close()


# -- audio


def write_tone(path, seconds):
    """Write `seconds` of the tone to `path`, as raw S16LE stereo at TONE_RATE."""
    step = 2 * math.pi * TONE_FREQUENCY / TONE_RATE
    samples = array.array('h')
    for i in range(int(seconds * TONE_RATE)):
        value = int(TONE_AMPLITUDE * math.sin(i * step))
        samples.append(value)
        samples.append(value)
    if sys.byteorder != 'little':
        samples.byteswap()
    with open(path, 'wb') as f:
        samples.tofile(f)


def play_tone(work, name, prefix=()):
    """Start `pacat` playing the tone into the default sink, its output in `work`/`name`.log; return it. `prefix`
    goes before the pacat command, to run it as another user, say. With a short latency, pacat drains the stream
    and exits once the sink has played the tone's last sample. PULSE_SERVER must name the test's own sound
    server, in the environment or in `prefix`: without it libpulse would find the desktop's."""
    if not os.environ.get('PULSE_SERVER') and not any(arg.startswith('PULSE_SERVER=') for arg in prefix):
        raise SystemExit('play_tone: PULSE_SERVER does not name a sound server; refusing to use the default one')
    path = os.path.join(work, f'{name}.raw')
    write_tone(path, TONE_SECONDS)
    command = list(prefix) + ['pacat', '--playback', '--raw', '--format=s16le', f'--rate={TONE_RATE}',
                              '--channels=2', '--latency-msec=50', '--client-name=andris-e2e-tone']
    with open(path, 'rb') as tone, open(os.path.join(work, f'{name}.log'), 'wb') as out:
        return subprocess.Popen(command, stdin=tone, stdout=out, stderr=subprocess.STDOUT)


def playback_stats(control):
    """The `playback` object of ryll's stats, or None when it has none."""
    return control.call('stats').get('playback')


def poll_playback(control, before, what, bound, test, watch):
    """Poll ryll's playback stats every AUDIO_POLL until test(playback) is true, and return them. Fails if the
    object goes, on a decode failure, or on a second START since `before`."""
    deadline = time.monotonic() + bound
    while True:
        playback = playback_stats(control)
        if playback is None:
            raise Failure('ryll\'s stats lost their playback object: the playback channel closed')
        if playback['decode_failures'] > before['decode_failures']:
            raise Failure(f'ryll failed to decode audio packets; playback stats: {playback}')
        if playback['starts'] > before['starts'] + 1:
            raise Failure(f'more than one START arrived for one tone; playback stats: {playback}')
        if test(playback):
            return playback
        watch.check()
        if time.monotonic() >= deadline:
            raise Failure(f'{what} within {bound:.1f} s; last playback stats: {playback}')
        time.sleep(AUDIO_POLL)


def check_audio(control, work, watch, name='tone', prefix=()):
    """A tone played into the default sink reaches ryll as one Opus stream that decodes, and stops once the tone
    ends; return a summary. `name` and `prefix` are for play_tone."""
    if control.protocol < AUDIO_PROTOCOL:
        wanted = '.'.join(str(part) for part in AUDIO_PROTOCOL)
        raise Failure(f'ryll speaks control protocol {".".join(str(p) for p in control.protocol)}; {wanted} or '
                      'later is needed for the playback stats')
    deadline = time.monotonic() + PLAYBACK_BOUND
    before = playback_stats(control)
    while before is None:
        watch.check()
        if time.monotonic() >= deadline:
            raise Failure(f'ryll\'s stats had no playback object within {PLAYBACK_BOUND:.0f} s: no playback '
                          'channel linked')
        time.sleep(AUDIO_POLL)
        before = playback_stats(control)
    if before['starts'] != before['stops'] or before['codec'] is not None:
        raise Failure(f'audio was already playing before the tone; playback stats: {before}')

    player = play_tone(work, name, prefix)
    try:
        started = time.monotonic()
        playing = poll_playback(control, before, 'no playback START arrived', AUDIO_START_BOUND,
                                lambda p: p['starts'] > before['starts'] and p['codec'] is not None, watch)
        to_start = time.monotonic() - started
        stream = (playing['codec'], playing['sample_rate'], playing['channels'])
        if stream != ('opus', TONE_RATE, 2):
            raise Failure(f'the tone arrived as {stream[0]} at {stream[1]} Hz in {stream[2]} channels, not opus at '
                          f'{TONE_RATE} Hz in 2; playback stats: {playing}')

        def drained(playback):
            if playback['stops'] > before['stops']:
                raise Failure(f'a STOP arrived while the tone played; playback stats: {playback}')
            return player.poll() is not None
        poll_playback(control, before, 'pacat did not finish the tone', TONE_BOUND, drained, watch)
        ended = time.monotonic()
        if player.returncode != 0:
            raise Failure(f'pacat exited {player.returncode}; see {name}.log')
        played = ended - started
    finally:
        if player.poll() is None:
            player.kill()
            player.wait()

    stopped = poll_playback(control, before, 'no playback STOP arrived after the tone ended', AUDIO_STOP_BOUND,
                            lambda p: p['stops'] > before['stops'], watch)
    to_stop = time.monotonic() - ended
    decoded = stopped['packets_decoded'] - before['packets_decoded']
    if stopped['starts'] != before['starts'] + 1 or stopped['stops'] != before['stops'] + 1:
        raise Failure(f'the tone gave {stopped["starts"] - before["starts"]} STARTs and '
                      f'{stopped["stops"] - before["stops"]} STOPs, not one of each; playback stats: {stopped}')
    if decoded < AUDIO_PACKETS:
        raise Failure(f'ryll decoded {decoded} packets of a {TONE_SECONDS:.0f} s tone, fewer than {AUDIO_PACKETS}; '
                      f'playback stats: {stopped}')
    if stopped['codec'] is not None:
        raise Failure(f'ryll still reports a {stopped["codec"]} stream after the STOP; playback stats: {stopped}')
    received = stopped['bytes_received'] - before['bytes_received']
    return (f'one START {to_start:.2f} s after the tone began, {stream[0]} at {stream[1]} Hz in {stream[2]} '
            f'channels; {decoded} packets ({received} bytes) decoded, no decode failures; pacat played for '
            f'{played:.2f} s, and the STOP arrived {to_stop:.2f} s after it exited')


def check_no_audio(control, watch):
    """With an andris run with --no-audio, ryll's stats never have a playback object."""
    if control.protocol < AUDIO_PROTOCOL:
        raise Failure('ryll\'s control protocol is too old to report a playback channel')
    deadline = time.monotonic() + NO_AUDIO_SPELL
    polls = 0
    while time.monotonic() < deadline:
        playback = playback_stats(control)
        polls += 1
        if playback is not None:
            raise Failure(f'ryll linked a playback channel to an andris without audio; playback stats: {playback}')
        watch.check()
        time.sleep(POLL)
    log(f'no-audio: {polls} polls of stats over {NO_AUDIO_SPELL:.0f} s showed no playback channel')


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--socket', required=True, help="ryll's control socket")
    parser.add_argument('--keylog', help="the keylogger's output file (required unless --no-stream)")
    parser.add_argument('--width', type=int, default=1024)
    parser.add_argument('--height', type=int, default=768)
    parser.add_argument('--display', required=True, help='the X display andris exports, for xclip')
    parser.add_argument('--andris-log', required=True, help="andris's log")
    parser.add_argument('--ryll-log', required=True, help="ryll's log, written with --verbose")
    parser.add_argument('--work', required=True, help='where the video script writes its log and last frame')
    parser.add_argument('--no-stream', action='store_true',
                        help='for a ryll started with --no-h264: check only that the video never streams')
    parser.add_argument('--no-audio', action='store_true',
                        help='for an andris started with --no-audio: check only that ryll has no playback channel')
    parser.add_argument('--kerbside', action='store_true',
                        help='for a ryll connected through kerbside: skip the idle, small-change and resize checks')
    parser.add_argument('--watch', action='append', default=[], metavar='NAME=PID',
                        help='a process that must stay up; fail at once if it exits (repeatable)')
    args = parser.parse_args()
    if not (args.no_stream or args.no_audio) and not args.keylog:
        parser.error('--keylog is required unless --no-stream or --no-audio')
    watch = Watch(args.watch)

    keylogger = keylogger_module()
    colour = keylogger.COLOUR
    deadline = time.monotonic() + SURFACE_BOUND
    control = None
    try:
        control = connect(args.socket, deadline, watch)
        surface = wait_surface(control, deadline, args.width, args.height, watch)
        wait_frame(control, surface['surface_id'], colour, watch)
        if args.no_stream:
            check_no_stream(control, args.display, args.work, watch)
            log('PASS')
            return 0
        if args.no_audio:
            check_no_audio(control, watch)
            log('PASS')
            return 0
        events = ['surface_drawn', 'clipboard_changed']
        subscribed = control.call('subscribe', {'events': events})['subscribed']
        if set(events) - set(subscribed):
            raise Failure(f'ryll would not subscribe to {events}: {subscribed}')
        send_keys(control, args.keylog, watch)
        if not args.kerbside:
            check_idle(control, watch)
            check_small_change(control, surface['surface_id'], keylogger, args.height, watch)
        check_clipboard(control, args.display, watch)
        if not args.kerbside:
            check_resize(control, (args.width, args.height), args.andris_log, args.ryll_log, watch)
        check_video(control, surface['surface_id'], args.display, args.work, args.andris_log, watch)
        log(f'stream reports: {report_summary(args.andris_log)}')
        log(f'audio: {check_audio(control, args.work, watch)}')
    except (Failure, RuntimeError, EOFError, OSError, subprocess.SubprocessError) as e:
        log(f'FAIL: {e}')
        return 1
    finally:
        if control is not None:
            control.close()
    log('PASS')
    return 0


if __name__ == '__main__':
    sys.exit(main())
