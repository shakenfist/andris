#!/usr/bin/env python3
"""The xrdp test bed's interleaving checks, X1 to X12.

scripts/xrdp-testbed/run.sh runs this inside the andris-xrdp-testbed
container, as root, once its smoke checks have passed: andris shares the
xrdp session, ryll --headless is connected to andris, and FreeRDP is
connected to xrdp. It refuses to run anywhere else. Keys reach the session
two ways:

  RDP     xdotool types on FreeRDP's own X display, into the FreeRDP
          window, which sends them to xrdp and xorgxrdp's keyboard;
  SPICE   ryll's control socket sends set-1 scancodes, which andris
          injects through XTEST.

The keylogger (scripts/e2e-keylogger.py --detail) is the whole session, and
prints every key it is given with its keysyms, keycode and state. Every
wait is on its output, on ryll's control socket, on the X servers or on a
log, with a bound; none is a blind sleep.

The checks, each printing PASS or FAIL with what it saw:

  X1  a, b, c and d, alternating RDP and SPICE, 20 times: each key reaches
      the keylogger once, in order, with the right keysym.
  X2  Shift held on one side while x is typed on the other, both ways.
      What the X server makes of that is recorded, not assumed; the check
      asserts that each x arrives once, and that once Shift is released
      neither side's next key is shifted.
  X3  Caps Lock from RDP, then a ryll restart, whose inputs channel sends
      KEY_MODIFIERS as it opens. Asserted: the RDP key turns Caps Lock on,
      and ryll's synchronisation turns it off again, since it came last
      (decision 6). Recorded: whether FreeRDP synchronises again when its
      window takes the focus, and what ryll's idle keepalive does to a
      lock state RDP set.
  X4  The FreeRDP window resized, so that xrdp resizes the session through
      RandR: ryll gets a surface of the new size, and a screenshot of it.
  X5  FreeRDP killed, 5 s without it, and started again: andris and ryll
      stay connected throughout, the session survives, and keys from both
      sides still arrive.
  X6  FreeRDP reconnected with a German layout: SPICE scancodes for y and z
      type the German keysyms, z and y, from the same keycodes as the RDP
      keys at the same places.
  X8  Two lines copied on FreeRDP's display, with xclip, reach ryll's
      clipboard through xrdp's chansrv and andris, with the LF line ends
      they started with: RDP carries them as CRLF, and andris, which
      promises the client LF, converts them back.
  X9  Two lines set with ryll's `clipboard_set` can be pasted, with xclip,
      on FreeRDP's display.
  X10 No loop. scripts/e2e-pasteboard.py stands in for a Mac's shared
      pasteboard, joining FreeRDP's display and ryll, so that each copy can
      come back into the session through the other client. After a copy in
      the session, and then one on FreeRDP's display, andris logs one
      clipboard crossing for each, and none more in the next LOOP_WINDOW,
      though RDP brings each copy's two lines back with CRLF line ends;
      then ryll and FreeRDP's display have the copy, with LF line ends, and
      the session has it with LF or, when chansrv owns CLIPBOARD, CRLF.
  X11 A resize from ryll is refused, since xorgxrdp does not allow clients
      to resize it: andris logs the refusal once for two requests, ryll's
      surface keeps the session's size, and a resize from RDP afterwards
      still reaches ryll.
  X12 scripts/e2e-video.py plays a moving picture in the session, which
      xorgxrdp also sends to FreeRDP: ryll's `stats` show one H.264 stream
      created and decoding frames, with no decode failure, and it is
      destroyed once the picture stops. RDP is unaffected: FreeRDP stays
      connected throughout, and logs no error while the picture plays.
  X7  The session ended, by ending its window manager (the keylogger):
      andris exits within 10 s, logging that the desktop has gone. Andris
      is run.sh's child, so run.sh checks its exit status afterwards.

The checks run in that order, and X7 runs even when an earlier one failed.
FreeRDP and chansrv cache clipboard text, so every copy is of text not used
before.
Before X1 the lock keys are aligned: FreeRDP's client display gets Num Lock
on and Caps Lock off, the state ryll asserts, so that neither client's
synchronisation flips the session's locks while keys are being counted.

Exit status: 0 when every check passed, 1 otherwise.
"""

import argparse
import base64
import collections
import importlib.util
import os
import re
import shlex
import signal
import subprocess
import sys
import time

from Xlib import X
from Xlib import display as xdisplay
from Xlib import error as xerror

MARKER = '/etc/andris-xrdp-testbed'
PREFIX = '[e2e-xrdp]'
SCRIPTS = os.path.dirname(os.path.abspath(__file__))

# Bounds, in seconds.
KEY_BOUND = 5.0         # a key, from being sent to the keylogger printing it
QUIET = 0.5             # how long without a key counts as no more keys coming
LOCK_BOUND = 5.0        # a lock state change, on either X server
PROCESS_BOUND = 10.0    # a process stopping when asked
RECONNECT_BOUND = 30.0  # FreeRDP, from starting to being connected to the session
RESIZE_BOUND = 15.0     # the session and ryll's surface, from the window resize
KEEPALIVE_BOUND = 15.0  # ryll's idle keepalive, which fires after 10 s without traffic
GONE_BOUND = 10.0       # andris exiting once the session has ended
CLIPBOARD_BOUND = 15.0  # a copy on one side, to the text being on the other
LOOP_WINDOW = 5.0       # X10: how long after a copy's first crossing to count further ones
RDP_GAP = 5.0           # X5: how long FreeRDP stays away
POLL = 0.1

# Set-1 make codes for the keys the checks send over SPICE.
SCANCODES = {
    'a': 0x1E, 'b': 0x30, 'c': 0x2E, 'd': 0x20, 'x': 0x2D, 'y': 0x15, 'z': 0x2C,
    'Shift_L': 0x2A, 'Caps_Lock': 0x3A, 'Num_Lock': 0x45,
}

# Lock keys arrive whenever a client synchronises the session's locks to
# its own; the key checks note them but do not count them.
LOCK_KEYS = {'Caps_Lock', 'Num_Lock', 'Scroll_Lock'}

# Core protocol state bits. Num Lock is Mod2 under both xorgxrdp's keymap
# and Xvfb's.
SHIFT_MASK = 0x01
LOCK_MASK = 0x02
NUM_MASK = 0x10

# The sizes X11 asks for over SPICE, which xorgxrdp refuses.
REFUSED_SIZES = ((1152, 720), (800, 600))

# Andris's info lines for clipboard text crossing (andris/src/server/agent.rs):
# the desktop's text offered to the client, and the client's put on the
# desktop.
OFFERED = "offering the desktop's clipboard text to the client"
PUT = "putting the client's clipboard text on the desktop"
CROSSED = f'{OFFERED}|{PUT}'

# X4's size, and X6's layout (FreeRDP's /kbd:layout takes a Windows layout
# id; 0x407 is German).
RESIZE = (1024, 768)
GERMAN = '/kbd:layout:0x407'

# xrdp's log line for each RDP connection reaching the session's Xorg.
CONNECTED = re.compile(r'lib_mod_log_peer: .* connected to Xorg_pid=')
# A colour escape, in case a log has them.
ESCAPE = re.compile(r'\x1b\[[0-9;]*m')


def log(message):
    print(f'{PREFIX} {message}', flush=True)


def load(name, filename):
    """Import one of the e2e scripts, whose file names are not identifiers."""
    spec = importlib.util.spec_from_file_location(name, os.path.join(SCRIPTS, filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


drive = load('e2e_drive', 'e2e-drive.py')
pasteboard_module = load('e2e_pasteboard', 'e2e-pasteboard.py')
Failure = drive.Failure

# What a check can fail with: its own Failure, ryll refusing a request
# (RuntimeError), a socket or process error, the X connection breaking, or
# a process missing from the watch list (KeyError) because an earlier check
# failed between stopping it and starting its replacement.
ERRORS = (Failure, RuntimeError, EOFError, OSError, KeyError, subprocess.SubprocessError, xerror.XError,
          xerror.ConnectionClosedError)


def alive(pid):
    """True while pid runs; a zombie counts as gone."""
    try:
        with open(f'/proc/{pid}/stat', encoding='utf-8') as f:
            state = f.read().rsplit(')', 1)[1].split()[0]
    except (OSError, IndexError):
        return False
    return state not in ('Z', 'X')


def wait_for(what, bound, test, watch=None):
    """Poll test() until it returns something true, and return that."""
    deadline = time.monotonic() + bound
    while True:
        result = test()
        if result:
            return result
        if watch is not None:
            watch.check()
        if time.monotonic() >= deadline:
            raise Failure(f'timed out after {bound:.0f} s waiting for {what}')
        time.sleep(POLL)


def describe(keys):
    """Keylogger entries, briefly: each key's name, with its state."""
    return ' '.join(f'{k["name"]}(0x{k["state"]:x})' for k in keys) or 'nothing'


def locks(mask):
    return f'Caps Lock {"on" if mask & LOCK_MASK else "off"}, Num Lock {"on" if mask & NUM_MASK else "off"}'


class Keylog:
    """The keylogger's output, read from a mark onwards."""

    def __init__(self, path):
        self.path = path
        self.pos = 0
        # Lock keys and keymap changes seen since the last mark.
        self.noise = []

    def entries(self):
        with open(self.path, encoding='utf-8', errors='replace') as f:
            lines = [line.split() for line in f]
        start = next((i for i, words in enumerate(lines) if words == ['READY']), None)
        if start is None:
            raise Failure(f'the keylogger never printed READY in {self.path}')
        result = []
        for words in lines[start + 1:]:
            if not words or words[0] not in ('KEY', 'MAPPING'):
                continue
            entry = dict(word.split('=', 1) for word in words[1:])
            entry['kind'] = words[0]
            if entry['kind'] == 'KEY':
                entry['keycode'] = int(entry['keycode'])
                entry['state'] = int(entry['state'], 16)
            result.append(entry)
        return result

    def mark(self):
        self.pos = len(self.entries())
        self.noise = []

    def _new_keys(self):
        new = self.entries()[self.pos:]
        keys = [e for e in new if e['kind'] == 'KEY' and e['name'] not in LOCK_KEYS]
        return new, keys

    def take(self, count, watch, bound=KEY_BOUND):
        """Wait for `count` keys after the mark, other than lock keys; return them and move the mark."""
        deadline = time.monotonic() + bound
        while True:
            new, keys = self._new_keys()
            if len(keys) >= count:
                break
            watch.check()
            if time.monotonic() >= deadline:
                raise Failure(f'the keylogger printed {describe(keys)} within {bound:.0f} s, '
                              f'expected {count} key(s)')
            time.sleep(POLL / 2)
        self.pos += len(new)
        self.noise.extend(e for e in new if e not in keys)
        return keys

    def quiet(self, watch):
        """Wait QUIET seconds, and return any keys other than lock keys that arrived."""
        deadline = time.monotonic() + QUIET
        while time.monotonic() < deadline:
            watch.check()
            time.sleep(POLL)
        new, keys = self._new_keys()
        self.pos += len(new)
        self.noise.extend(e for e in new if e not in keys)
        return keys

    def noted(self):
        """The lock keys and keymap changes seen since the mark, for a report."""
        counts = collections.Counter(e['name'] if e['kind'] == 'KEY' else 'MappingNotify' for e in self.noise)
        return ', '.join(f'{count} {name}' for name, count in sorted(counts.items())) or 'none'


class Bed:
    """The test bed's processes and displays, and the two ways to type."""

    def __init__(self, args):
        self.args = args
        self.watch = drive.Watch([f'andris={args.andris}', f'ryll={args.ryll}', f'freerdp={args.freerdp}'])
        self.keylog = Keylog(os.path.join(args.work, 'keylogger.log'))
        self.control = None
        self.children = []
        self.restarts = collections.Counter()
        os.environ['XAUTHORITY'] = args.xauthority
        self.session = xdisplay.Display(args.session)
        self.client = xdisplay.Display(args.client_display)
        self.window = None
        self.decoy = self._decoy()

    def close(self):
        if self.control is not None:
            self.control.close()
        for child in self.children:
            if child.poll() is None:
                child.kill()
                child.wait()

    # -- the session's X server

    def session_mask(self):
        return self.session.screen().root.query_pointer().mask

    def session_size(self):
        geometry = self.session.screen().root.get_geometry()
        return geometry.width, geometry.height

    def wait_session_locks(self, mask, what):
        """Wait for the session's Caps Lock and Num Lock to be as in `mask`."""
        want = mask & (LOCK_MASK | NUM_MASK)
        try:
            wait_for(what, LOCK_BOUND, lambda: self.session_mask() & (LOCK_MASK | NUM_MASK) == want, self.watch)
        except Failure:
            raise Failure(f'timed out after {LOCK_BOUND:.0f} s waiting for {what}: wanted {locks(want)}, but the '
                          f'session has {locks(self.session_mask())}') from None

    def layout(self):
        query = subprocess.run(['setxkbmap', '-display', self.args.session, '-query'], capture_output=True,
                               text=True, check=True, timeout=10).stdout
        found = re.search(r'^layout:\s*(\S+)', query, re.MULTILINE)
        return found.group(1) if found else '?'

    # -- the RDP side

    def xdotool(self, *args):
        env = {'DISPLAY': self.args.client_display, 'PATH': os.environ['PATH']}
        return subprocess.run(['xdotool', *args], env=env, capture_output=True, text=True, check=True,
                              timeout=10).stdout

    def _decoy(self):
        """A tiny window on the client's display, away from FreeRDP's, to take the focus from it."""
        screen = self.client.screen()
        window = screen.root.create_window(screen.width_in_pixels - 4, screen.height_in_pixels - 4, 4, 4, 0,
                                           screen.root_depth, X.InputOutput, X.CopyFromParent,
                                           override_redirect=True)
        window.map()
        self.client.sync()
        wait_for('the decoy window to map', LOCK_BOUND,
                 lambda: window.get_attributes().map_state == X.IsViewable)
        return window

    def find_window(self):
        pid = self.watch.pids['freerdp']

        def search():
            try:
                return self.xdotool('search', '--onlyvisible', '--pid', str(pid)).split()
            except subprocess.CalledProcessError:
                return None
        self.window = wait_for(f'a FreeRDP window (pid {pid})', RECONNECT_BOUND, search, self.watch)[0]
        return self.window

    def focus_rdp(self):
        """Give FreeRDP the focus, which makes it synchronise the session's locks to the client's."""
        self.xdotool('windowfocus', '--sync', self.window)

    def unfocus_rdp(self):
        self.decoy.set_input_focus(X.RevertToParent, X.CurrentTime)
        self.client.sync()

    def client_mask(self):
        return self.client.screen().root.query_pointer().mask

    def set_client_lock(self, key, mask, on):
        """Make one of the client display's locks `on`, without FreeRDP seeing it. Leaves FreeRDP unfocused."""
        if bool(self.client_mask() & mask) == on:
            return
        self.unfocus_rdp()
        self.xdotool('key', key)
        wait_for(f'{key} {"on" if on else "off"} on the client display', LOCK_BOUND,
                 lambda: bool(self.client_mask() & mask) == on)

    def align_locks(self):
        """Num Lock on and Caps Lock off, on the client and in the session: the state ryll asserts."""
        self.set_client_lock('Num_Lock', NUM_MASK, True)
        self.set_client_lock('Caps_Lock', LOCK_MASK, False)
        self.unfocus_rdp()
        self.focus_rdp()
        try:
            self.wait_session_locks(NUM_MASK, "FreeRDP to synchronise the session's locks")
        except Failure:
            # FreeRDP did not synchronise; put the session right over SPICE.
            mask = self.session_mask()
            if mask & LOCK_MASK:
                self.spice('Caps_Lock')
            if not mask & NUM_MASK:
                self.spice('Num_Lock')
            self.wait_session_locks(NUM_MASK, "SPICE lock keys to align the session's locks")

    def rdp(self, key, action='key'):
        self.xdotool(action, key)

    def stop_freerdp(self):
        pid = self.watch.pids.pop('freerdp')
        os.kill(pid, signal.SIGTERM)
        wait_for(f'FreeRDP (pid {pid}) to exit', PROCESS_BOUND, lambda: not alive(pid), self.watch)

    def start_freerdp(self, *extra):
        before = self.connections()
        command = shlex.split(self.args.freerdp_command) + list(extra)
        child = self.spawn('freerdp', command)
        self.watch.pids['freerdp'] = child.pid
        wait_for('FreeRDP to connect to the session', RECONNECT_BOUND, lambda: self.connections() > before,
                 self.watch)
        self.find_window()
        self.align_locks()

    def connections(self):
        with open(self.args.xrdp_log, encoding='utf-8', errors='replace') as f:
            return sum(1 for line in f if CONNECTED.search(line))

    # -- the SPICE side

    def spice(self, key, state='press'):
        self.control.call('send_key', {'scancode': SCANCODES[key], 'state': state})

    def connect_ryll(self):
        """Connect to ryll's control socket, retrying while a ryll just started is not yet answering."""
        deadline = time.monotonic() + drive.SURFACE_BOUND
        while self.control is None:
            try:
                self.control = drive.connect(self.args.socket, deadline, self.watch)
            except (OSError, EOFError) as e:
                if time.monotonic() >= deadline:
                    raise Failure(f'could not talk to ryll on {self.args.socket}: {e}') from None
                time.sleep(POLL)
        subscribed = self.control.call('subscribe', {'events': ['clipboard_changed']})['subscribed']
        if 'clipboard_changed' not in subscribed:
            raise Failure(f'ryll would not subscribe to clipboard_changed: {subscribed}')
        width, height = self.session_size()
        return drive.wait_surface(self.control, deadline, width, height, self.watch)

    def ryll_log(self):
        """The log of the ryll running now: run.sh's first, or the newest the checks started."""
        if self.restarts['ryll']:
            return os.path.join(self.args.work, f'ryll-{self.restarts["ryll"]}.log')
        return os.path.join(self.args.work, 'ryll.log')

    def andris_log(self):
        return os.path.join(self.args.work, 'andris.log')

    def freerdp_log(self):
        """The log of the FreeRDP running now: run.sh's first, or the newest the checks started."""
        if self.restarts['freerdp']:
            return os.path.join(self.args.work, f'freerdp-{self.restarts["freerdp"]}.log')
        return os.path.join(self.args.work, 'freerdp.log')

    def restart_ryll(self):
        self.control.close()
        self.control = None
        pid = self.watch.pids.pop('ryll')
        os.kill(pid, signal.SIGTERM)
        wait_for(f'ryll (pid {pid}) to exit', PROCESS_BOUND, lambda: not alive(pid), self.watch)
        # So that the connection below reaches the new ryll, not the old one's socket.
        if os.path.exists(self.args.socket):
            os.unlink(self.args.socket)
        child = self.spawn('ryll', shlex.split(self.args.ryll_command))
        self.watch.pids['ryll'] = child.pid
        self.connect_ryll()

    # -- either side

    def type(self, key, side):
        if side == 'RDP':
            self.rdp(key)
        else:
            self.spice(key)

    def spawn(self, name, command):
        self.restarts[name] += 1
        path = os.path.join(self.args.work, f'{name}-{self.restarts[name]}.log')
        with open(path, 'wb') as out:
            child = subprocess.Popen(command, stdout=out, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                                     start_new_session=True)
        self.children.append(child)
        log(f'started {name} (pid {child.pid}), logging to {os.path.basename(path)}')
        return child


def expect_one(keys, name, what):
    if len(keys) != 1 or keys[0]['name'] != name:
        raise Failure(f'{what}: the keylogger saw {describe(keys)}, expected {name}')
    return keys[0]


def x1(bed):
    """a, b, c, d alternating RDP and SPICE, 20 times."""
    sequence = [('a', 'RDP'), ('b', 'SPICE'), ('c', 'RDP'), ('d', 'SPICE')] * 20
    bed.keylog.mark()
    for i, (key, side) in enumerate(sequence):
        bed.type(key, side)
        expect_one(bed.keylog.take(1, bed.watch), key, f'key {i + 1} of {len(sequence)} ({key} from {side})')
    extra = bed.keylog.quiet(bed.watch)
    if extra:
        raise Failure(f'after the {len(sequence)} keys, the keylogger also printed {describe(extra)}')
    return (f'{len(sequence)} keys, alternating RDP and SPICE, each arrived once, in order, with the right keysym '
            f'(lock keys and keymap changes on the way: {bed.keylog.noted()})')


def typed(key):
    """What a KEY entry typed: its shifted keysym when Shift was down."""
    return key['shifted'] if key['state'] & SHIFT_MASK else key['name']


def release(what, action):
    """Run a check's cleanup, logging rather than raising its failure, so that it cannot hide the failure that
    led to it. A cleanup that fails after a check passed shows up in the next check, as a stuck key."""
    try:
        action()
    except ERRORS as e:
        log(f'NOTE: could not {what}: {e!r}')


def x2(bed):
    """Shift held on one side, x typed on the other, both ways."""
    bed.keylog.mark()
    observed = []
    try:
        bed.rdp('Shift_L', 'keydown')
        expect_one(bed.keylog.take(1, bed.watch), 'Shift_L', 'RDP Shift down')
        bed.spice('x')
        key = expect_one(bed.keylog.take(1, bed.watch), 'x', 'SPICE x with RDP Shift held')
        observed.append(f'RDP Shift + SPICE x typed {typed(key)} (state 0x{key["state"]:x})')
    finally:
        release('release the RDP Shift', lambda: bed.rdp('Shift_L', 'keyup'))
    try:
        bed.spice('Shift_L', 'down')
        expect_one(bed.keylog.take(1, bed.watch), 'Shift_L', 'SPICE Shift down')
        bed.rdp('x')
        key = expect_one(bed.keylog.take(1, bed.watch), 'x', 'RDP x with SPICE Shift held')
        observed.append(f'SPICE Shift + RDP x typed {typed(key)} (state 0x{key["state"]:x})')
    finally:
        release('release the SPICE Shift', lambda: bed.spice('Shift_L', 'up'))
    for side in ('RDP', 'SPICE'):
        bed.type('x', side)
        key = expect_one(bed.keylog.take(1, bed.watch), 'x', f'{side} x after both Shifts were released')
        if key['state'] & SHIFT_MASK:
            raise Failure(f'{"; ".join(observed)}; but once Shift was released, {side} x still typed '
                          f'{typed(key)} (state 0x{key["state"]:x}): a Shift is stuck')
    return f'{"; ".join(observed)}; after release, x from each side typed x, unshifted'


def x3(bed):
    """Caps Lock from RDP, then a ryll restart, which synchronises the locks."""
    notes = []
    bed.keylog.mark()
    if bed.session_mask() & LOCK_MASK:
        raise Failure('Caps Lock was already on in the session before the check')

    # RDP turns Caps Lock on, in the session and on its own display.
    bed.rdp('Caps_Lock')
    bed.wait_session_locks(LOCK_MASK | NUM_MASK, "RDP Caps Lock to turn the session's Caps Lock on")
    bed.spice('a')
    key = expect_one(bed.keylog.take(1, bed.watch), 'a', 'SPICE a with RDP Caps Lock on')
    notes.append(f"RDP Caps Lock turned the session's on, and SPICE a then typed with state 0x{key['state']:x}")

    # Ryll synchronises as its inputs channel opens, with Caps Lock off.
    bed.restart_ryll()
    bed.wait_session_locks(NUM_MASK, "ryll's KEY_MODIFIERS to turn the session's Caps Lock off")
    notes.append("a restarted ryll's KEY_MODIFIERS turned it off again (it synchronised last)")

    # Recorded: does FreeRDP synchronise again when its window takes the
    # focus? Its own display still has Caps Lock on.
    bed.unfocus_rdp()
    bed.focus_rdp()
    try:
        bed.wait_session_locks(LOCK_MASK | NUM_MASK, 'FreeRDP')
        log("NOTE: X3: refocusing FreeRDP, whose display has Caps Lock on, turned the session's back on: "
            'FreeRDP synchronises on focus, and then it was last')
        refocus = 'FreeRDP synchronised again on focus and won'
    except Failure:
        log(f'NOTE: X3: refocusing FreeRDP left the session with {locks(bed.session_mask())}')
        refocus = 'refocusing FreeRDP did not synchronise'
        bed.rdp('Caps_Lock')
        bed.wait_session_locks(LOCK_MASK | NUM_MASK, "RDP Caps Lock to turn the session's Caps Lock on")

    # Recorded: ryll's idle keepalive restates the KEY_MODIFIERS it last sent
    # once its inputs channel has been quiet for 10 s.
    bed.spice('a')
    bed.keylog.take(1, bed.watch)
    start = time.monotonic()
    try:
        wait_for("the session's Caps Lock to change", KEEPALIVE_BOUND,
                 lambda: not bed.session_mask() & LOCK_MASK, bed.watch)
        elapsed = time.monotonic() - start
        log(f'NOTE: X3: with Caps Lock on from RDP, an idle ryll turned it off {elapsed:.1f} s after its last key: '
            'its keepalive restates KEY_MODIFIERS, which andris applies like any other')
        keepalive = f"ryll's idle keepalive turned Caps Lock off {elapsed:.1f} s after its last key"
    except Failure:
        log(f'NOTE: X3: the session kept Caps Lock on for {KEEPALIVE_BOUND:.0f} s with ryll idle')
        keepalive = f'Caps Lock stayed on for {KEEPALIVE_BOUND:.0f} s with ryll idle'

    bed.align_locks()
    return f'{"; ".join(notes)}. Recorded: {refocus}; {keepalive}'


def screenshot_size(bed, surface_id):
    result = bed.control.call('screenshot', {'surface_id': surface_id, 'format': 'rgba'})
    size = (result['width'], result['height'])
    if len(base64.b64decode(result['data_base64'])) != size[0] * size[1] * 4:
        raise Failure(f'the {size[0]}x{size[1]} screenshot has the wrong number of bytes')
    return size


def wait_ryll_surface(bed, size):
    """Wait for ryll to have a surface of `size`, and check a screenshot of it has that size too."""
    def surface():
        for found in bed.control.call('status').get('surfaces', []):
            if (found['width'], found['height']) == size:
                return found
        return None
    try:
        found = wait_for(f'a {size[0]}x{size[1]} surface in ryll', RESIZE_BOUND, surface, bed.watch)
    except Failure:
        raise Failure(f'the session is {size[0]}x{size[1]}, but ryll has '
                      f'{bed.control.call("status").get("surfaces")}') from None
    shot = screenshot_size(bed, found['surface_id'])
    if shot != size:
        raise Failure(f'ryll has a {size[0]}x{size[1]} surface, but its screenshot is {shot[0]}x{shot[1]}')


def rdp_resize(bed, size):
    """Resize the FreeRDP window, so that xrdp resizes the session, and wait for ryll to follow. Return the
    session's size before."""
    before = bed.session_size()
    bed.xdotool('windowsize', '--sync', bed.window, str(size[0]), str(size[1]))
    try:
        wait_for(f'the session to resize from {before[0]}x{before[1]}', RESIZE_BOUND,
                 lambda: bed.session_size() != before and bed.session_size(), bed.watch)
    except Failure:
        raise Failure(f'the FreeRDP window is {size[0]}x{size[1]}, but the session stayed '
                      f'{before[0]}x{before[1]}') from None
    # The session may pass through other sizes on the way.
    wait_for(f'the session to reach {size[0]}x{size[1]}', RESIZE_BOUND, lambda: bed.session_size() == size,
             bed.watch)
    wait_ryll_surface(bed, size)
    return before


def x4(bed):
    """The FreeRDP window resized, so that xrdp resizes the session."""
    before = rdp_resize(bed, RESIZE)
    return (f'the session went from {before[0]}x{before[1]} to {RESIZE[0]}x{RESIZE[1]}, and ryll got a surface '
            f'and a screenshot of that size')


def x5(bed):
    """FreeRDP away for 5 s, then back."""
    andris, ryll = bed.watch.pids['andris'], bed.watch.pids['ryll']
    keylogger = keylogger_pid()
    bed.stop_freerdp()
    deadline = time.monotonic() + RDP_GAP
    polls = 0
    while time.monotonic() < deadline:
        bed.watch.check()
        if not bed.control.call('status').get('spice_connected'):
            raise Failure(f'ryll lost its SPICE session {RDP_GAP - (deadline - time.monotonic()):.1f} s '
                          'after FreeRDP went')
        polls += 1
        time.sleep(POLL * 2.5)
    size = bed.session_size()
    bed.start_freerdp()
    for name, pid in (('andris', andris), ('ryll', ryll)):
        if not alive(pid):
            raise Failure(f'{name} (pid {pid}) exited while FreeRDP was away')
        if bed.watch.pids.get(name) != pid:
            raise Failure(f'{name} is now pid {bed.watch.pids.get(name)}, not pid {pid}, which was running '
                          'before FreeRDP went')
    if not bed.control.call('status').get('spice_connected'):
        raise Failure('ryll is not connected once FreeRDP is back')
    if keylogger_pid() != keylogger:
        raise Failure("the session's window manager, the keylogger, is not the one that was running")
    bed.keylog.mark()
    for key, side in (('a', 'RDP'), ('b', 'SPICE')):
        bed.type(key, side)
        expect_one(bed.keylog.take(1, bed.watch), key, f'{key} from {side} after the reconnect')
    after = bed.session_size()
    wait_ryll_surface(bed, after)
    return (f'ryll stayed connected through {polls} status polls in {RDP_GAP:.0f} s without FreeRDP; FreeRDP '
            f"reconnected to the same session ({size[0]}x{size[1]} before, {after[0]}x{after[1]} after, and ryll's "
            'surface followed); a from RDP and b from SPICE both arrived')


def x6(bed):
    """FreeRDP reconnected with a German layout."""
    before = bed.layout()
    bed.stop_freerdp()
    bed.start_freerdp(GERMAN)
    after = bed.layout()
    bed.keylog.mark()
    seen = {}
    for key in ('y', 'z'):
        for side in ('RDP', 'SPICE'):
            bed.type(key, side)
            keys = bed.keylog.take(1, bed.watch)
            if len(keys) != 1:
                raise Failure(f'{key} from {side}: the keylogger saw {describe(keys)}')
            seen[key, side] = keys[0]
    report = ', '.join(f'{side} {key} -> {k["name"]} (keycode {k["keycode"]})' for (key, side), k in seen.items())
    report = f"the session's layout went from {before} to {after}; {report}; keymap changes: {bed.keylog.noted()}"
    wrong = [f'SPICE {key} typed {seen[key, "SPICE"]["name"]}, not {german}'
             for key, german in (('y', 'z'), ('z', 'y')) if seen[key, 'SPICE']['name'] != german]
    wrong += [f'SPICE {key} came from keycode {seen[key, "SPICE"]["keycode"]}, RDP {key} from '
              f'{seen[key, "RDP"]["keycode"]}' for key in ('y', 'z')
              if seen[key, 'SPICE']['keycode'] != seen[key, 'RDP']['keycode']]
    if wrong:
        raise Failure(f'{"; ".join(wrong)}. Saw: {report}')
    return report


def two_lines(label):
    """Text on two lines, each ending in LF, unique to `label` and this run, with multi-byte characters."""
    return f'andris e2e {label} {os.getpid()} {time.time_ns()}\nsecond line: héllo wörld, 世界 🎉\n'


def line_ends(got, text):
    """How `got` relates to `text`, which ends its lines in LF."""
    if got == text:
        return 'LF line ends, unchanged'
    if got == text.replace('\n', '\r\n'):
        return 'CRLF line ends'
    return f'{len((got or "").encode())} other bytes'


def x8(bed):
    """Two lines copied on FreeRDP's display reach ryll, with LF line ends."""
    text = two_lines('x8-rdp-to-spice')
    before = len(drive.log_lines(bed.andris_log(), OFFERED))
    mark = len(bed.control.events)
    start = time.monotonic()
    copier = drive.xclip_copy(bed.args.client_display, text.encode('utf-8'))
    try:
        bed.control.wait_event('clipboard_changed', lambda data: True, CLIPBOARD_BOUND, bed.watch, start=mark)
        elapsed = time.monotonic() - start
        got = bed.control.call('clipboard_get').get('text')
    finally:
        drive.stop(copier)
    if got != text:
        raise Failure(f'two lines copied on FreeRDP\'s display reached ryll with {line_ends(got, text)}')
    if len(drive.log_lines(bed.andris_log(), OFFERED)) <= before:
        raise Failure(f'ryll has the text, but andris never logged "{OFFERED}"')
    return (f'two lines, {len(text.encode())} bytes, copied on FreeRDP\'s display reached ryll after '
            f'{elapsed:.2f} s, with LF line ends')


def x9(bed):
    """Two lines set in ryll can be pasted on FreeRDP's display."""
    text = two_lines('x9-spice-to-rdp')
    before = len(drive.log_lines(bed.andris_log(), PUT))
    elapsed = drive.spice_to_x(bed.control, bed.args.client_display, text, bed.watch, CLIPBOARD_BOUND)
    if len(drive.log_lines(bed.andris_log(), PUT)) <= before:
        raise Failure(f'FreeRDP\'s display has the text, but andris never logged "{PUT}"')
    return (f'two lines, {len(text.encode())} bytes, set in ryll were pasted on FreeRDP\'s display after '
            f'{elapsed:.2f} s')


def crossings(bed):
    """Andris's log lines for clipboard text crossing, either way."""
    return drive.log_lines(bed.andris_log(), CROSSED)


def paste_stepping(display, pasteboard):
    """CLIPBOARD on `display`, pasted with xclip while the pasteboard runs, since it may be the owner, or the
    owner may ask it; or None if there is no owner or it refuses."""
    child = subprocess.Popen(['xclip', '-display', display, '-selection', 'clipboard', '-o', '-t', 'UTF8_STRING'],
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + CLIPBOARD_BOUND
    while child.poll() is None:
        if time.monotonic() >= deadline:
            drive.stop(child)
            return None
        pasteboard.step(POLL / 2)
    data = child.stdout.read()
    child.stdout.close()
    return data if child.returncode == 0 else None


def step_until(what, bound, test, bed, pasteboard):
    """wait_for, but running the pasteboard while it waits."""
    deadline = time.monotonic() + bound
    while not test():
        bed.watch.check()
        if time.monotonic() >= deadline:
            raise Failure(f'timed out after {bound:.0f} s waiting for {what}')
        pasteboard.step(POLL)


def crossing(bed, text, display, pasteboard):
    """Copy `text` on `display`; wait for andris to log a crossing, then LOOP_WINDOW more, running the
    pasteboard throughout. Return andris's crossings since the copy, the seconds to the first, and how many
    times the pasteboard passed text on."""
    mark = len(crossings(bed))
    passes = pasteboard.passes()
    pasteboard.log(f'copying {pasteboard_module.describe(text.encode("utf-8"))} on {display}')
    copier = drive.xclip_copy(display, text.encode('utf-8'))
    try:
        start = time.monotonic()
        step_until(f'andris to log the crossing of the copy on {display}', CLIPBOARD_BOUND,
                   lambda: len(crossings(bed)) > mark, bed, pasteboard)
        first = time.monotonic() - start
        deadline = time.monotonic() + LOOP_WINDOW
        while time.monotonic() < deadline:
            bed.watch.check()
            pasteboard.step(POLL)
        return crossings(bed)[mark:], first, pasteboard.passes() - passes
    finally:
        drive.stop(copier)


def once(bed, label, display, pasteboard):
    """Two lines copied on `display` cross once, and then everything has them."""
    text = two_lines(label)
    data = text.encode('utf-8')
    lines, first, passed = crossing(bed, text, display, pasteboard)
    if len(lines) != 1:
        raise Failure(f'after a copy on {display}, andris logged {len(lines)} crossings within '
                      f'{first + LOOP_WINDOW:.1f} s, not 1, and the pasteboard passed text {passed} times: '
                      'the clipboard loops')
    got = bed.control.call('clipboard_get').get('text')
    if got != text:
        raise Failure(f'after the copy on {display} settled, ryll has {line_ends(got, text)}')
    pasted = paste_stepping(bed.args.client_display, pasteboard)
    if pasted != data:
        raise Failure(f'after the copy on {display} settled, a paste on FreeRDP\'s display gave '
                      f'{line_ends(pasted.decode("utf-8", "replace") if pasted else None, text)}')
    # What the session's X clients get is xrdp's to decide when chansrv
    # owns CLIPBOARD there: it serves the CRLF it was given.
    pasted = paste_stepping(bed.args.session, pasteboard)
    if pasted not in (data, text.replace('\n', '\r\n').encode('utf-8')):
        raise Failure(f'after the copy on {display} settled, a paste in the session gave '
                      f'{line_ends(pasted.decode("utf-8", "replace") if pasted else None, text)}')
    session = line_ends(pasted.decode('utf-8'), text)
    kind = 'offered to ryll' if OFFERED in lines[0] else 'put on the desktop'
    return (f'two lines copied on {display} crossed once ({kind}, {first:.2f} s after the copy) and not again '
            f'in {LOOP_WINDOW:.0f} s; ryll and FreeRDP\'s display have them with LF line ends, the session with '
            f'{session}; the pasteboard passed text {passed} times')


def x10(bed):
    """No loop through a shared pasteboard joining FreeRDP's display and ryll."""
    path = os.path.join(bed.args.work, 'pasteboard.log')
    with open(path, 'a', encoding='utf-8') as out:
        def note(line):
            print(f'{time.monotonic():.3f} {line}', file=out, flush=True)
        pasteboard = pasteboard_module.Pasteboard(bed.args.client_display, bed.control, note)
        try:
            session = once(bed, 'x10-session', bed.args.session, pasteboard)
            client = once(bed, 'x10-client', bed.args.client_display, pasteboard)
        finally:
            pasteboard.close()
    return f'{session}; {client}'


def x11(bed):
    """A resize from ryll is refused, logged once, and an RDP resize still reaches ryll."""
    size = bed.session_size()
    ryll_log = bed.ryll_log()
    before = drive.refusals(ryll_log)
    for asked in REFUSED_SIZES:
        drive.request_size(bed.control, asked)
        before = drive.wait_refused(ryll_log, before, bed.watch)
        now = drive.surface_size(bed.control)
        if now != size or bed.session_size() != size:
            raise Failure(f'after a refused resize to {asked[0]}x{asked[1]}, the session is '
                          f'{bed.session_size()[0]}x{bed.session_size()[1]} and ryll\'s surface {now[0]}x{now[1]}, '
                          f'not {size[0]}x{size[1]}')
    lines = drive.log_lines(bed.andris_log(), drive.REFUSED)
    if len(lines) != 1:
        raise Failure(f'andris logged {len(lines)} refusals for {len(REFUSED_SIZES)} refused resizes, not 1: '
                      f'{lines}')
    target = RESIZE if size != RESIZE else (1152, 864)
    rdp_resize(bed, target)
    asked = ' and '.join(f'{w}x{h}' for w, h in REFUSED_SIZES)
    return (f'{asked} from ryll were refused, logged once ({drive.refusal_reason(lines[0])}); ryll\'s surface '
            f'stayed {size[0]}x{size[1]}; an RDP resize to {target[0]}x{target[1]} then reached ryll')


# FreeRDP's log lines for an error, as WLog writes them.
FREERDP_ERROR = '[ERROR]'


def x12(bed):
    """A video in the session streams to ryll as H.264, and RDP is unaffected."""
    freerdp_log = bed.freerdp_log()
    with open(freerdp_log, 'rb') as f:
        mark = len(f.read())
    video = drive.Video(bed.args.session, bed.args.work, 'video', bed.watch)
    try:
        start = time.monotonic()
        stats = drive.poll_stats(bed.control, 'no H.264 stream decoded frames', drive.STREAM_BOUND,
                                 drive.h264_decoding, bed.watch)
        stream = drive.h264_decoding(stats)[0]
        started = time.monotonic() - start
        time.sleep(drive.STREAM_SPELL)
        later = drive.poll_stats(bed.control, 'stats could not be read', 1.0, lambda d: True, bed.watch)
        live = [s for s in later['streams'] if s['stream_id'] == stream['stream_id']]
        first = stream['frames_decoded_ok']
        if later['streams_created_total'] != 1 or not live or live[0]['frames_decoded_ok'] <= first:
            raise Failure(f'the video did not play as one decoding stream for {drive.STREAM_SPELL:.0f} s; '
                          f'stats: {later}')
        video.stop(bed.watch)
        stopped = time.monotonic()
        drive.poll_stats(bed.control, 'the stream was not destroyed', drive.DESTROY_BOUND,
                         lambda d: d['streams_destroyed_total'] >= 1 and not d['streams'], bed.watch)
        ended = time.monotonic() - stopped
    finally:
        video.close()
    bed.watch.check()
    with open(freerdp_log, 'rb') as f:
        f.seek(mark)
        errors = [line for line in f.read().decode('utf-8', 'replace').splitlines() if FREERDP_ERROR in line]
    if errors:
        raise Failure(f'FreeRDP logged {len(errors)} error(s) while the video played: {errors[:3]}')
    rects = drive.stream_rects(bed.andris_log())
    return (f'an H.264 stream ({stream["width"]}x{stream["height"]}, andris logged '
            f'{", ".join(drive.describe_rect(r) for r in rects)}) was decoding after {started:.1f} s, decoded '
            f'{live[0]["frames_decoded_ok"]} frames with no failure, and was destroyed {ended:.2f} s after the '
            f'video stopped; FreeRDP stayed up, and logged no error')


def x7(bed):
    """End the session by ending its window manager, the keylogger."""
    andris = bed.watch.pids['andris']
    keylogger = keylogger_pid()
    bed.session.close()
    os.kill(keylogger, signal.SIGTERM)
    start = time.monotonic()
    try:
        wait_for('andris to exit', GONE_BOUND, lambda: not alive(andris))
    except Failure:
        raise Failure(f'andris (pid {andris}) was still running {GONE_BOUND:.0f} s after the session ended') from None
    elapsed = time.monotonic() - start
    path = os.path.join(bed.args.work, 'andris.log')
    with open(path, encoding='utf-8', errors='replace') as f:
        lines = [ESCAPE.sub('', line).strip() for line in f if 'the desktop has gone' in line]
    if not lines:
        raise Failure(f'andris exited {elapsed:.1f} s after the session ended, but its log never says '
                      '"the desktop has gone"')
    return f'andris exited {elapsed:.1f} s after the session ended, logging: {lines[0]}'


def keylogger_pid():
    for entry in os.listdir('/proc'):
        if not entry.isdigit():
            continue
        try:
            with open(f'/proc/{entry}/cmdline', 'rb') as f:
                argv = f.read().split(b'\0')
        except OSError:
            continue
        if any(arg.endswith(b'e2e-keylogger.py') for arg in argv) and alive(int(entry)):
            return int(entry)
    raise Failure('the keylogger is not running')


def setup(bed):
    bed.connect_ryll()
    bed.find_window()
    bed.align_locks()
    return f'the session has {locks(bed.session_mask())}, as ryll asserts, and FreeRDP has the focus'


CHECKS = (('X1', x1), ('X2', x2), ('X3', x3), ('X4', x4), ('X5', x5), ('X6', x6), ('X8', x8), ('X9', x9),
          ('X10', x10), ('X11', x11), ('X12', x12))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--socket', required=True, help="ryll's control socket")
    parser.add_argument('--session', required=True, help="the xrdp session's display")
    parser.add_argument('--client-display', required=True, help="FreeRDP's own display")
    parser.add_argument('--work', required=True, help="run.sh's work directory, holding the logs")
    parser.add_argument('--xauthority', required=True, help="the session's Xauthority file")
    parser.add_argument('--xrdp-log', required=True, help="xrdp's log")
    parser.add_argument('--andris', type=int, required=True, help="andris's pid")
    parser.add_argument('--ryll', type=int, required=True, help="ryll's pid")
    parser.add_argument('--freerdp', type=int, required=True, help="FreeRDP's pid")
    parser.add_argument('--ryll-command', required=True, help='the command that started ryll, shell-quoted')
    parser.add_argument('--freerdp-command', required=True, help='the command that started FreeRDP, shell-quoted')
    args = parser.parse_args()
    if not os.path.exists(MARKER):
        log(f'refusing to run: no {MARKER}, so this is not the andris-xrdp-testbed container')
        return 2

    failures = 0

    def run(name, check, *args):
        nonlocal failures
        try:
            log(f'PASS: {name}: {check(*args)}')
        except ERRORS as e:
            log(f'FAIL: {name}: {e if isinstance(e, Failure) else repr(e)}')
            failures += 1

    try:
        bed = Bed(args)
    except ERRORS as e:
        log(f"FAIL: could not reach the test bed's X servers, so no check ran: {e!r}")
        return 1
    try:
        run('setup', setup, bed)
        if not failures:
            for name, check in CHECKS:
                run(name, check, bed)
        # X7 runs whatever happened before it, since run.sh expects andris to have exited.
        run('X7', x7, bed)
    finally:
        bed.close()
    return 1 if failures else 0


if __name__ == '__main__':
    sys.exit(main())
