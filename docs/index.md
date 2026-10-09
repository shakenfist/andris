# Andris Documentation

## What is Andris?

Andris is a planned SPICE server, written in Rust, that exports an
existing X11 desktop -- such as the Xorg session behind an xrdp
connection -- so that it can be reached from the
[ryll](https://github.com/shakenfist/ryll) SPICE client, directly
or through the [kerbside](https://github.com/shakenfist/kerbside)
SPICE proxy.

Andris has no server yet: the repository holds a stub binary and
the build and CI scaffold around it. This documentation consists of
how to build and test andris, and the plans that describe what it
is meant to become; pages describing how to configure and run it
will be added here as the software they describe is written.

## Documentation Index

- [Development](development.md) - Building, testing, pre-commit, CI
  and the merge queue
- [Plans index](plans/index.md) - Every planning document, with its
  intent and status
- [X11 desktop over SPICE](plans/PLAN-x11-desktop.md) - The master
  plan: why andris exists, its design commitments, and the order in
  which it will be built

## Project Files

- [README](https://github.com/shakenfist/andris/blob/develop/README.md) - What andris is and its current status
- [ARCHITECTURE](https://github.com/shakenfist/andris/blob/develop/ARCHITECTURE.md) - The repository's shape today and its relationship to ryll and kerbside
- [AGENTS](https://github.com/shakenfist/andris/blob/develop/AGENTS.md) - Guide for AI coding assistants
