# Andris - A Rust SPICE Server for an Existing X11 Desktop

Andris shares a Linux desktop that is *already running* over SPICE
(Simple Protocol for Independent Computing Environments). It attaches
to an existing X11 display and serves it to a SPICE client, in
particular [ryll](https://github.com/shakenfist/ryll), optionally
through the [kerbside](https://github.com/shakenfist/kerbside) SPICE
proxy. It is written in Rust, and aimed at people who reach a Linux
machine's desktop remotely and want to do so with a SPICE client.

Andris connects to the display as an ordinary X client, so it works
with any X server -- Xorg, the xorgxrdp session behind an xrdp
connection, or Xvfb -- and it never starts a desktop of its own. That
sets it apart from its neighbours: Xspice is an Xorg driver that
starts a new display, and QEMU's SPICE server shows a virtual
machine's screen. Its closest relative is x11spice. Its main use is
the session behind xrdp, so that RDP and SPICE can show the same
desktop at once.

Andris does not support Wayland yet. Pointed at an XWayland display,
it would see only the X applications on it, not the Wayland desktop.

Andris is a sibling of ryll: it builds on ryll's shared
`shakenfist-spice-*` protocol and compression crates, and sends only
message types that kerbside's allowlist accepts.

## Status

Andris is pre-alpha. It exports an X display to one client at a
time, with the main, display, inputs and cursor channels: you get the
desktop, a keyboard, a mouse, a pointer shape, text clipboard and
resizing where the X server allows it. There is no video or audio
yet. Build it with `make release`
and run it against a display:

```bash
DISPLAY=:10 target/release/andris --ticket-file ticket
```

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

Project reference files:

- [ARCHITECTURE.md](https://github.com/shakenfist/andris/blob/develop/ARCHITECTURE.md) - Andris's components, data flow and relationship to ryll and kerbside
- [AGENTS.md](https://github.com/shakenfist/andris/blob/develop/AGENTS.md) - Guide for AI coding assistants

## License

Apache-2.0
