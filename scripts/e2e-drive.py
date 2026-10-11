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
              prints `a` and then `Left` within KEY_BOUND.

Every wait gives up at once if a process named with --watch (andris, ryll,
the keylogger) exits, rather than running out its bound.

The control-socket client follows ryll's tools/proxmox-smoke.py: NDJSON
requests with an integer id, responses matched by id, events skipped. See
ryll's docs/control-socket-protocol.md. `send_key` takes the logical set-1
make code, with the 0xE0 prefix in the high byte; protocol 1.2 is required,
because older servers sent extended keys with the bytes the wrong way round.

Exit status: 0 when every check passed, 1 otherwise.
"""

import argparse
import base64
import importlib.util
import json
import os
import socket
import sys
import time

PREFIX = '[e2e-drive]'

# From starting: the control socket must accept a connection, and status
# must show a surface, within SURFACE_BOUND.
SURFACE_BOUND = 30.0

# From the first surface: a screenshot must show the keylogger's colour.
FRAME_BOUND = 10.0

# From the key presses: the keylogger must have printed both keysyms.
KEY_BOUND = 5.0

POLL = 0.25

# The keys sent, as (logical set-1 make code, the keysym name the keylogger
# must print).
KEYS = ((0x1E, 'a'), (0xE04B, 'Left'))


def log(message):
    print(f'{PREFIX} {message}', flush=True)


def keylogger_colour():
    """Import COLOUR from the keylogger, so the oracle has one definition."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'e2e-keylogger.py')
    spec = importlib.util.spec_from_file_location('e2e_keylogger', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.COLOUR


class Control:
    """A minimal client for ryll's control socket (see the module docstring)."""

    def __init__(self, path, timeout=10.0):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.buf = bytearray()
        self.next_id = 0

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
        hello = control.call('hello', {'client_name': 'andris-e2e', 'protocol_version': '1.2'})
        version = tuple(int(part) for part in hello['protocol_version'].split('.'))
        if version < (1, 2):
            raise Failure(f'ryll speaks control protocol {hello["protocol_version"]}; 1.2 or later is '
                          'needed for extended keys')
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


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--socket', required=True, help="ryll's control socket")
    parser.add_argument('--keylog', required=True, help="the keylogger's output file")
    parser.add_argument('--width', type=int, default=1024)
    parser.add_argument('--height', type=int, default=768)
    parser.add_argument('--watch', action='append', default=[], metavar='NAME=PID',
                        help='a process that must stay up; fail at once if it exits (repeatable)')
    args = parser.parse_args()
    watch = Watch(args.watch)

    colour = keylogger_colour()
    deadline = time.monotonic() + SURFACE_BOUND
    control = None
    try:
        control = connect(args.socket, deadline, watch)
        surface = wait_surface(control, deadline, args.width, args.height, watch)
        wait_frame(control, surface['surface_id'], colour, watch)
        send_keys(control, args.keylog, watch)
    except (Failure, RuntimeError, EOFError, OSError) as e:
        log(f'FAIL: {e}')
        return 1
    finally:
        if control is not None:
            control.close()
    log('PASS')
    return 0


if __name__ == '__main__':
    sys.exit(main())
