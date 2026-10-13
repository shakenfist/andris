# AGENTS.md - Guide for AI Coding Assistants

Conventions and gotchas for working in andris that you cannot infer
by reading the repository. Everything else is documented elsewhere;
this file points you there rather than restating it.

## What andris is

Andris is a Linux-only SPICE *server*, written in Rust, that
exports an existing X11 desktop (such as an xrdp session's Xorg
server) to SPICE clients. It serves the main, display, inputs and
cursor channels to one client at a time; what it does not yet do, and
what it is meant to become, is in the master plan,
[`docs/plans/PLAN-x11-desktop.md`](docs/plans/PLAN-x11-desktop.md).

Describe what exists. Every document outside `docs/plans/` states
the current state of the repository, and points at the master plan
for intent. Never write planned behaviour as if it already exists,
and never cite plan phase numbers outside `docs/plans/`.

Related repositories:

- **shakenfist/ryll** -- the SPICE client that is andris's reference
  client, and the home of the shared `shakenfist-spice-*` crates
  andris builds on
- **shakenfist/kerbside** -- the SPICE proxy that is meant to sit
  between ryll and andris

## Where the documentation lives

| Question | Document |
|----------|----------|
| What is in the repository, and how does it relate to ryll and kerbside? | [`ARCHITECTURE.md`](ARCHITECTURE.md) |
| What documentation exists? | [`docs/index.md`](docs/index.md) |
| How do I run andris and connect to it? | [`docs/running.md`](docs/running.md) |
| How do I build, test and land a change? | [`docs/development.md`](docs/development.md) |
| What is andris for, and how will it be built? | [`docs/plans/PLAN-x11-desktop.md`](docs/plans/PLAN-x11-desktop.md) |
| What has been planned, and what is its status? | [`docs/plans/index.md`](docs/plans/index.md) |

Links inside `docs/` must resolve within `docs/`. Anything outside
it -- source files, workflows, `README.md` -- needs an absolute
`https://github.com/shakenfist/andris/blob/develop/<path>` URL, as
does every link in `README.md`. The fleet's consistency audit checks
both.

## Invariants

These hold from the first line of code onwards. Each comes from the
master plan, which gives the reasoning.

- **Andris only emits SPICE message types that ryll models.**
  Kerbside's allowlist terminates a session on any message type
  that ryll's `logging::message_names` tables (in
  `shakenfist-spice-protocol`) do not name. Sending only what ryll
  names keeps andris kerbside-compatible by construction. A new
  message type is added to ryll first.
- **No code is translated from x11spice or spice-server.**
  x11spice is GPLv3 and spice-server (libspice-server) is LGPL;
  andris is Apache-2.0. Both are references for *behaviour only*:
  read them to learn what a server does, then write it afresh.
- **Andris must not depend on `shakenfist-spice-renderer`.** That
  crate is ryll's client substrate. Wire types andris needs belong
  in `shakenfist-spice-protocol`, and encoders in
  `shakenfist-spice-compression`.

## Protocol reference sources

When working on SPICE protocol details, these sources are available
locally:

| Source | Path | Use for |
|--------|------|---------|
| ryll shared crates | `shakenfist/ryll/shakenfist-spice-*/` | Link handshake and ticket auth for the server role (`link.rs`), message constants, the `logging::message_names` tables, image decoders to round-trip encoders against |
| Kerbside proxy | `shakenfist/kerbside/rust/kerbside-proxy/` | The capabilities offered to clients (`caps.rs`) and the per-channel message allowlist (`allowlist.rs`) |
| SPICE protocol headers | `/srv/src-reference/spice/spice-protocol/` | Canonical enum definitions, message structures, capability flags |
| SPICE common library | `/srv/src-reference/spice/spice-common/` | Marshalling code shared by server and client |
| spice-server | `/srv/src-reference/spice/spice/` | Reference server implementation (LGPL; behaviour only) |
| x11spice | `/srv/src-reference/spice/x11spice/` | Reference X11 desktop exporter (GPLv3; behaviour only) |
| QEMU | `/srv/src-reference/qemu/qemu/` | How a production server drives libspice-server, in `ui/spice-*` |

## Working conventions

- **Cargo runs through the `Makefile`, inside Docker, never on the
  host.** Use `make build`,
  `make test` and `make lint`, which wrap cargo in a devcontainer as
  ryll's do. Do not install a Rust toolchain on the host.
- **`make e2e` runs andris against Xvfb and ryll's headless client.**
  It needs nothing on the host beyond Docker, and runs in CI's merge
  tier.
- **Tests involving xrdp run on the test bed** (`make e2e-xrdp`,
  session on `:50`), never against a live session.
- **Server opcodes come from the per-channel enums in
  `server/opcodes.rs`**, never raw numbers. Its test checks that
  ryll names every variant.
- **Keys cross the platform seam as Linux evdev codes**, not SPICE
  scancodes or X keycodes.
- **`platform/` carries no SPICE or ryll types.** Mapping to and from
  SPICE happens in `server/`.
- **No `unsafe` outside `platform/x11/shm.rs`.** The crate has
  `#![deny(unsafe_code)]`; that module alone opts out, for the
  MIT-SHM mapping, and every `unsafe` block there carries a `SAFETY:`
  comment.
- **No `unwrap()` in production code.** `clippy::unwrap_used` is on
  for the workspace; tests may unwrap.
- **Commits go through `pre-commit run --all-files`.** Run it, and
  fix what it reports, before proposing a commit.
- **Python, if any appears** (tooling under `tools/`, say), uses
  single quotes for strings, double quotes for docstrings, and lines
  wrapped at 120 characters.
- **Human review tracking is deliberately absent.** Andris carries
  no `REVIEWS.md`, `.vscode/review-scope.toml` or prune-reviews
  workflow. Do not copy them across from ryll.

## Process documents

Two process documents at the repository root capture workflows we
use repeatedly. Read the relevant one before starting.

- **`PLAN-TEMPLATE.md`** -- the starting point for new plan files.
  Plans live in `docs/plans/`, follow this template, and are
  registered in `docs/plans/index.md`. Phase plans are named
  `PLAN-<name>-phase-NN-<description>.md` beside their master plan.
- **`PUSH-AUDIT.md`** -- the pre-push audit for our own branches:
  a mechanical wave, then parallel judgment sub-agents. Run it
  before pushing a branch that will become a pull request, and as
  the last phase of every master plan, over the whole plan's work.
