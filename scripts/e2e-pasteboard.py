"""A stand-in for a Mac's shared pasteboard, for the xrdp test bed's check X10.

On the operator's Mac, the RDP client and ryll share one pasteboard: text
copied in either reaches the other, and so, through xrdp and through andris,
the same desktop twice. Two clipboard bridges into one desktop make a loop,
which andris breaks by dropping text identical to the last text that
crossed. Pasteboard makes the same loop on the test bed, between FreeRDP's
own X display and ryll's control socket:

  - it watches CLIPBOARD on FreeRDP's display with XFixes, and when another
    program takes it, fetches its text and gives it to ryll's
    `clipboard_set`, as the Mac's pasteboard passes the RDP client's copy
    to ryll;
  - for each `clipboard_changed` from ryll, it reads the text with
    `clipboard_get` and takes CLIPBOARD on FreeRDP's display with it, as
    the pasteboard passes ryll's copy to the RDP client, which FreeRDP then
    offers to xrdp.

It ignores its own ownership, and compares no content: whatever stops the
loop, it must not be this. It does drop the answer to a fetch from an owner
it has since displaced: FreeRDP answers a request it had when it lost
CLIPBOARD with bytes that are not the text (seen as 87 bytes unlike the
copy, then a lone 0x02), and a real pasteboard, holding one text at a time,
never asks an owner it has replaced. Ryll's control socket takes one client at a time,
so scripts/e2e-xrdp-drive.py imports this and shares its own connection with
it, calling step() whenever it would otherwise wait. Nothing here runs
anything by itself.
"""

import select
import time

from Xlib import X
from Xlib import Xatom
from Xlib import display as xdisplay
from Xlib.ext import xfixes
from Xlib.protocol import event as xevent


def text_hash(data):
    """The 32-bit FNV-1a hash of bytes `data`, as eight hex digits: andris's debug logs carry the same, so a text
    can be followed through both without logging it."""
    value = 0x811c9dc5
    for byte in data:
        value = ((value ^ byte) * 0x01000193) & 0xffffffff
    return f'{value:08x}'


def describe(data):
    """`data`'s length and hash, for a log line."""
    return f'{len(data)} bytes, hash {text_hash(data)}'


class Pasteboard:
    """The pasteboard, joining X display `display` and ryll's control connection `control`, which must be
    subscribed to `clipboard_changed`. `log` takes a line for each text passed either way."""

    def __init__(self, display, control, log):
        self.control = control
        self.log = log
        # How many times text was passed: to ryll, and to FreeRDP.
        self.to_ryll = 0
        self.to_rdp = 0
        self.d = xdisplay.Display(display)
        self.d.xfixes_query_version()
        screen = self.d.screen()
        self.window = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth)
        self.clipboard = self.d.intern_atom('CLIPBOARD')
        self.utf8 = self.d.intern_atom('UTF8_STRING')
        self.targets = self.d.intern_atom('TARGETS')
        self.prop = self.d.intern_atom('ANDRIS_E2E_PASTEBOARD')
        self.d.xfixes_select_selection_input(self.window, self.clipboard,
                                             xfixes.XFixesSetSelectionOwnerNotifyMask)
        self.d.flush()
        self.text = b''
        # Fetches from other owners still to be answered. Taking CLIPBOARD
        # abandons them.
        self.fetching = 0
        # Ryll's events before this one are not the pasteboard's business.
        self.seen = len(control.events)

    def close(self):
        self.d.close()

    def passes(self):
        return self.to_ryll + self.to_rdp

    def own(self, text):
        """Take CLIPBOARD on FreeRDP's display with `text`."""
        self.text = text
        self.fetching = 0
        self.window.set_selection_owner(self.clipboard, X.CurrentTime)
        self.d.flush()

    def x_events(self):
        while self.d.pending_events():
            e = self.d.next_event()
            # python-xlib makes a class per extension event, so compare by name.
            if type(e).__name__ == 'SetSelectionOwnerNotify':
                if e.owner and e.owner.id != self.window.id:
                    self.log(f'window 0x{e.owner.id:x} took CLIPBOARD on FreeRDP\'s display; asking for its text')
                    self.fetching += 1
                    self.window.convert_selection(self.clipboard, self.utf8, self.prop, X.CurrentTime)
                    self.d.flush()
            elif e.type == X.SelectionNotify and e.requestor.id == self.window.id:
                if not self.fetching:
                    value = self.window.get_full_property(self.prop, X.AnyPropertyType)
                    self.window.delete_property(self.prop)
                    self.d.flush()
                    late = bytes(value.value) if value else b''
                    self.log(f'dropped {describe(late)}: the answer of an owner the pasteboard has since replaced')
                    continue
                self.fetching -= 1
                if e.property == X.NONE:
                    self.log('the new owner of CLIPBOARD refused a paste')
                    continue
                value = self.window.get_full_property(self.prop, X.AnyPropertyType)
                self.window.delete_property(self.prop)
                self.d.flush()
                text = bytes(value.value) if value else b''
                if text:
                    self.to_ryll += 1
                    self.log(f'{describe(text)} on FreeRDP\'s display; giving them to ryll')
                    self.control.call('clipboard_set', {'text': text.decode('utf-8', 'replace')})
            elif e.type == X.SelectionRequest:
                self.serve(e)

    def serve(self, e):
        requestor, prop = e.requestor, e.property or e.target
        if e.target != self.targets:
            self.log(f'window 0x{requestor.id:x} asked for target {e.target}; serving {describe(self.text)}')
        if e.target == self.targets:
            requestor.change_property(prop, Xatom.ATOM, 32, [self.targets, self.utf8, Xatom.STRING])
        elif e.target in (self.utf8, Xatom.STRING):
            requestor.change_property(prop, e.target, 8, self.text)
        else:
            prop = X.NONE
        requestor.send_event(xevent.SelectionNotify(time=e.time, requestor=requestor, selection=e.selection,
                                                    target=e.target, property=prop))
        self.d.flush()

    def ryll_events(self):
        events = self.control.events
        while self.seen < len(events):
            _, name, data = events[self.seen]
            self.seen += 1
            if name != 'clipboard_changed':
                continue
            text = self.control.call('clipboard_get').get('text') or ''
            self.to_rdp += 1
            self.log(f'ryll has {data.get("bytes")} bytes, and gave {describe(text.encode("utf-8"))}; taking '
                     'CLIPBOARD on FreeRDP\'s display with them')
            self.own(text.encode('utf-8'))

    def step(self, seconds):
        """Pass on whatever arrives within `seconds`, from either side."""
        deadline = time.monotonic() + seconds
        while True:
            self.x_events()
            self.ryll_events()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            readable, _, _ = select.select([self.d.fileno(), self.control.sock], [], [], remaining)
            if self.control.sock in readable:
                self.control.pump(0.01)
