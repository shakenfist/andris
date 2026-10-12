#!/usr/bin/env python3
"""The xrdp test bed's smoke check of what ryll sees.

scripts/xrdp-testbed/run.sh runs this inside the andris-xrdp-testbed
container once andris shares the xrdp session and ryll --headless is
connected to it. It refuses to run anywhere else. Through ryll's control
socket it checks, printing PASS or FAIL for each:

  surface   ryll's `status` reports a surface of the session's size, and an
            RGBA `screenshot` of it has that size too;
  frame     the screenshot's centre pixel is the keylogger's colour, so a
            frame of the session's window reached ryll.

The control-socket client and its bounds are scripts/e2e-drive.py's,
imported rather than copied. Every wait gives up at once if a process named
with --watch exits.

Exit status: 0 when every check passed, 1 otherwise.
"""

import argparse
import base64
import importlib.util
import os
import sys
import time

MARKER = '/etc/andris-xrdp-testbed'
PREFIX = '[e2e-xrdp]'
SCRIPTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def log(message):
    print(f'{PREFIX} {message}', flush=True)


def load(name, filename):
    """Import one of the e2e scripts, whose file names are not identifiers."""
    spec = importlib.util.spec_from_file_location(name, os.path.join(SCRIPTS, filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def check_surface(drive, control, deadline, width, height, watch):
    surface = drive.wait_surface(control, deadline, width, height, watch)
    result = control.call('screenshot', {'surface_id': surface['surface_id'], 'format': 'rgba'})
    size = (result['width'], result['height'])
    data = base64.b64decode(result['data_base64'])
    if size != (width, height):
        raise drive.Failure(f'the screenshot is {size[0]}x{size[1]}, expected {width}x{height}')
    if len(data) != width * height * 4:
        raise drive.Failure(f'the rgba screenshot is {len(data)} bytes, expected {width * height * 4}')
    log(f'PASS: ryll has a {width}x{height} surface, and its screenshot is {width}x{height}')
    return surface


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--socket', required=True, help="ryll's control socket")
    parser.add_argument('--size', required=True, help="the xrdp session's size, as WIDTHxHEIGHT")
    parser.add_argument('--watch', action='append', default=[], metavar='NAME=PID',
                        help='a process that must stay up; fail at once if it exits (repeatable)')
    args = parser.parse_args()
    if not os.path.exists(MARKER):
        log(f'refusing to run: no {MARKER}, so this is not the andris-xrdp-testbed container')
        return 2
    width, height = (int(part) for part in args.size.split('x'))

    drive = load('e2e_drive', 'e2e-drive.py')
    keylogger = load('e2e_keylogger', 'e2e-keylogger.py')
    watch = drive.Watch(args.watch)
    deadline = time.monotonic() + drive.SURFACE_BOUND
    control = None
    try:
        control = drive.connect(args.socket, deadline, watch)
        surface = check_surface(drive, control, deadline, width, height, watch)
        drive.wait_frame(control, surface['surface_id'], keylogger.COLOUR, watch)
        log('PASS: a frame of the keylogger\'s window reached ryll')
    except (drive.Failure, RuntimeError, EOFError, OSError) as e:
        log(f'FAIL: {e}')
        return 1
    finally:
        if control is not None:
            control.close()
    return 0


if __name__ == '__main__':
    sys.exit(main())
