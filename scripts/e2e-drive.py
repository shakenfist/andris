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
              logs the refusal once, and ryll's surface keeps its size.

Every wait gives up at once if a process named with --watch (andris, ryll,
the keylogger) exits, rather than running out its bound.

The control-socket client follows ryll's tools/proxmox-smoke.py: NDJSON
requests with an integer id, responses matched by id, events skipped. See
ryll's docs/control-socket-protocol.md. `send_key` takes the logical set-1
make code, with the 0xE0 prefix in the high byte. Protocol 1.3 is required,
for the clipboard and resize verbs; 1.2 fixed extended keys, which older
servers sent with the bytes the wrong way round.

The resize checks read andris's log (--andris-log), and ryll's (--ryll-log),
whose debug lines record each agent reply andris sends.

Exit status: 0 when every check passed, 1 otherwise.
"""

import argparse
import base64
import importlib.util
import json
import os
import re
import select
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

# The control protocol needed: 1.3 added the clipboard and resize verbs.
PROTOCOL = (1, 3)

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
                          'needed for the clipboard and resize verbs')
        log(f'connected to {path}, protocol {hello["protocol_version"]}')
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


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--socket', required=True, help="ryll's control socket")
    parser.add_argument('--keylog', required=True, help="the keylogger's output file")
    parser.add_argument('--width', type=int, default=1024)
    parser.add_argument('--height', type=int, default=768)
    parser.add_argument('--display', required=True, help='the X display andris exports, for xclip')
    parser.add_argument('--andris-log', required=True, help="andris's log")
    parser.add_argument('--ryll-log', required=True, help="ryll's log, written with --verbose")
    parser.add_argument('--watch', action='append', default=[], metavar='NAME=PID',
                        help='a process that must stay up; fail at once if it exits (repeatable)')
    args = parser.parse_args()
    watch = Watch(args.watch)

    keylogger = keylogger_module()
    colour = keylogger.COLOUR
    deadline = time.monotonic() + SURFACE_BOUND
    control = None
    try:
        control = connect(args.socket, deadline, watch)
        surface = wait_surface(control, deadline, args.width, args.height, watch)
        wait_frame(control, surface['surface_id'], colour, watch)
        events = ['surface_drawn', 'clipboard_changed']
        subscribed = control.call('subscribe', {'events': events})['subscribed']
        if set(events) - set(subscribed):
            raise Failure(f'ryll would not subscribe to {events}: {subscribed}')
        send_keys(control, args.keylog, watch)
        check_idle(control, watch)
        check_small_change(control, surface['surface_id'], keylogger, args.height, watch)
        check_clipboard(control, args.display, watch)
        check_resize(control, (args.width, args.height), args.andris_log, args.ryll_log, watch)
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
