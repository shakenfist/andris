# Andris Documentation

## What is Andris?

Andris is a planned SPICE server, written in Rust, that exports an
existing X11 desktop -- such as the Xorg session behind an xrdp
connection -- so that it can be reached from the
[ryll](https://github.com/shakenfist/ryll) SPICE client, directly
or through the [kerbside](https://github.com/shakenfist/kerbside)
SPICE proxy.

Andris has no code yet. Today this documentation consists of the
plans that describe what andris is meant to become; pages describing
how to build, configure and run andris will be added here as the
software that they describe is written.

## Documentation Index

- [Plans index](plans/index.md) - Every planning document, with its
  intent and status
- [X11 desktop over SPICE](plans/PLAN-x11-desktop.md) - The master
  plan: why andris exists, its design commitments, and the order in
  which it will be built

## Project Files

- [README](https://github.com/shakenfist/andris/blob/develop/README.md) - What andris is and its current status
- [ARCHITECTURE](https://github.com/shakenfist/andris/blob/develop/ARCHITECTURE.md) - The repository's shape today and its relationship to ryll and kerbside
- [AGENTS](https://github.com/shakenfist/andris/blob/develop/AGENTS.md) - Guide for AI coding assistants
