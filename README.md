# Andris - A Rust SPICE Server for an Existing X11 Desktop

Andris is intended to be a SPICE (Simple Protocol for Independent
Computing Environments) server, written in Rust, that exports an
X11 desktop which is *already running* -- for example the Xorg
session behind an xrdp connection -- rather than starting a desktop
of its own. It is aimed at people who reach a Linux machine's
desktop remotely and want to do so with a SPICE client, in
particular [ryll](https://github.com/shakenfist/ryll), optionally
through the [kerbside](https://github.com/shakenfist/kerbside) SPICE
proxy.

Andris is a sibling of ryll: it is meant to build on ryll's shared
`shakenfist-spice-*` protocol and compression crates, and to send
only message types that kerbside's allowlist accepts.

## Status

Andris is pre-alpha. The repository holds its plans, a stub binary
that prints its name and version, and the build and CI scaffold
around it. There is no SPICE server yet, so there is nothing useful
to install or run. What andris is meant to become, and the order in
which it will be built, is set out in the
[X11 desktop over SPICE master plan](https://github.com/shakenfist/andris/blob/develop/docs/plans/PLAN-x11-desktop.md).

## Documentation

- [Documentation index](https://github.com/shakenfist/andris/blob/develop/docs/index.md) - Where andris's documentation lives
- [Plans index](https://github.com/shakenfist/andris/blob/develop/docs/plans/index.md) - Every planning document and its status
- [X11 desktop over SPICE](https://github.com/shakenfist/andris/blob/develop/docs/plans/PLAN-x11-desktop.md) - The master plan: what andris is for and how it will be built
- [Development](https://github.com/shakenfist/andris/blob/develop/docs/development.md) - Building, testing and the CI pipeline

Project reference files:

- [ARCHITECTURE.md](https://github.com/shakenfist/andris/blob/develop/ARCHITECTURE.md) - The repository's shape today and its relationship to ryll and kerbside
- [AGENTS.md](https://github.com/shakenfist/andris/blob/develop/AGENTS.md) - Guide for AI coding assistants

## License

Apache-2.0
