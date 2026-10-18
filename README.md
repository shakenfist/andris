# Andris - A Rust SPICE Server for an Existing X11 Desktop

Andris is a SPICE (Simple Protocol for Independent Computing
Environments) server, written in Rust, for a Linux desktop that is
*already running*. It attaches to an existing X11 display and serves
it to a SPICE client, in particular
[ryll](https://github.com/shakenfist/ryll), optionally through the
[kerbside](https://github.com/shakenfist/kerbside) SPICE proxy. It is
aimed at people who reach a Linux machine's desktop remotely, above
all the [session behind
xrdp](https://github.com/shakenfist/andris/blob/develop/docs/xrdp.md),
so that RDP and SPICE can show the same desktop at once.

Andris connects to the display as an ordinary X client and never
starts a desktop of its own. It works with an X server that has a
32-bit BGRX root window and the XTEST, XFIXES, XKEYBOARD and DAMAGE
extensions -- Xorg, the xorgxrdp session behind xrdp, or Xvfb, for
instance -- and [checks for
them](https://github.com/shakenfist/andris/blob/develop/docs/running.md#exporting-an-x-display)
before it listens. That sets it apart from its neighbours: Xspice is
an Xorg driver that starts a new display, and QEMU's SPICE server
shows a virtual machine's screen. Its closest relative is x11spice.

Andris does not support Wayland. It needs a real X11 desktop: an
XWayland display holds only the X applications of a Wayland session,
not the desktop itself.

Andris is a sibling of ryll: it builds on ryll's shared
`shakenfist-spice-*` protocol and compression crates, and sends only
message types that kerbside's allowlist accepts.

## Status

Andris is pre-alpha. It exports an X display to one client at a
time, with the main, display, inputs, cursor and playback channels:
you get the desktop, a keyboard, a mouse, a pointer shape, text
clipboard and resizing where the X server allows it, video areas as
H.264 streams to clients that support it, and the desktop's sound.
Install it from PyPI and run it against a display:

```bash
pipx install andris
(umask 077; head -c 24 /dev/urandom | base64 > ticket)
DISPLAY=:10 andris --ticket-file ticket
```

To run it as a service beside xrdp, `andris install-unit` sets up its
systemd unit; see
[Running andris alongside xrdp](https://github.com/shakenfist/andris/blob/develop/docs/xrdp.md).
To build it from source, see
[Running andris](https://github.com/shakenfist/andris/blob/develop/docs/running.md#installing-and-building).

What andris is meant to become, and the order in which it is being
built, is set out in the
[X11 desktop over SPICE master plan](https://github.com/shakenfist/andris/blob/develop/docs/plans/PLAN-x11-desktop.md).

## Documentation

- [Documentation index](https://github.com/shakenfist/andris/blob/develop/docs/index.md) - Where andris's documentation lives
- [Plans index](https://github.com/shakenfist/andris/blob/develop/docs/plans/index.md) - Every planning document and its status
- [X11 desktop over SPICE](https://github.com/shakenfist/andris/blob/develop/docs/plans/PLAN-x11-desktop.md) - The master plan: what andris is for and how it will be built
- [Running andris](https://github.com/shakenfist/andris/blob/develop/docs/running.md) - Options, the ticket file, TLS, connecting with ryll and fronting andris with kerbside
- [Running andris alongside xrdp](https://github.com/shakenfist/andris/blob/develop/docs/xrdp.md) - The systemd user unit, and how RDP and SPICE share a desktop
- [Development](https://github.com/shakenfist/andris/blob/develop/docs/development.md) - Building, testing and the CI pipeline
- [Releasing andris](https://github.com/shakenfist/andris/blob/develop/docs/releasing.md) - Publishing the wheels to PyPI

Project reference files:

- [ARCHITECTURE.md](https://github.com/shakenfist/andris/blob/develop/ARCHITECTURE.md) - Andris's components, data flow and relationship to ryll and kerbside
- [AGENTS.md](https://github.com/shakenfist/andris/blob/develop/AGENTS.md) - Guide for AI coding assistants

## License

Apache-2.0
