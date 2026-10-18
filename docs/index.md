# Andris Documentation

## What is Andris?

Andris is a SPICE server, written in Rust, that exports an
existing X11 desktop -- such as the Xorg session behind an xrdp
connection -- so that it can be reached from the
[ryll](https://github.com/shakenfist/ryll) SPICE client, directly
or through the [kerbside](https://github.com/shakenfist/kerbside)
SPICE proxy.

Andris serves the main, display, inputs, cursor and playback
channels to one client at a time, and shares text clipboards and
resizes the desktop through the main channel. Areas of the desktop
that play video go to clients that support it as H.264 streams, and
the desktop's sound goes over the playback channel. This documentation covers how to run
andris, how to build and test it, and the plans that describe what it
is meant to become.

## Documentation Index

- [Running andris](running.md) - Building, options, the ticket file,
  TLS, video streams, audio, metrics, connecting with ryll and fronting
  andris with kerbside
- [Running andris alongside xrdp](xrdp.md) - The systemd user unit, what
  happens when the session ends, how RDP and SPICE input,
  clipboard and resizing interact, and how to give both clients sound
- [Development](development.md) - Building, testing, pre-commit, CI
  and the merge queue
- [Releasing andris](releasing.md) - Publishing the wheels to PyPI:
  the one-time setup, the two-stage release, and what it produces
- [Plans index](plans/index.md) - Every planning document, with its
  intent and status
- [X11 desktop over SPICE](plans/PLAN-x11-desktop.md) - The master
  plan: why andris exists, its design commitments, and the order in
  which it will be built

## Project Files

- [README](https://github.com/shakenfist/andris/blob/develop/README.md) - What andris is and its current status
- [ARCHITECTURE](https://github.com/shakenfist/andris/blob/develop/ARCHITECTURE.md) - Andris's components, data flow and relationship to ryll and kerbside
- [AGENTS](https://github.com/shakenfist/andris/blob/develop/AGENTS.md) - Guide for AI coding assistants
