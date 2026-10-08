# Architecture

This document is the map of andris: what the repository holds, how
it relates to the repositories around it, and the shape it is meant
to grow into. The documentation index is
[`docs/index.md`](docs/index.md).

## The repository today

Andris holds a design and no code. There is no Cargo workspace, no
binary, no `Makefile` and no CI, so there are no components and no
data flow to describe yet. What exists is:

| Path | What it is |
|------|------------|
| `docs/plans/PLAN-x11-desktop.md` | The master plan: why andris exists, its design commitments, open questions and the order of work |
| `docs/plans/` (other files) | Phase plans for that master plan, and `index.md`, which lists every plan with its status |
| `docs/index.md` | The documentation index |
| `README.md` | The pitch and current status |
| `AGENTS.md` | Conventions for AI coding assistants |
| `PLAN-TEMPLATE.md`, `PUSH-AUDIT.md` | The planning template and the pre-push audit runbook |

When code lands, a component inventory replaces this table.

## Relationship to ryll and kerbside

Andris is the third member of a SPICE trio in the shakenfist
organisation, and most of its protocol knowledge is meant to come
from the other two rather than be written again here.

| Repository | Role | What andris takes from it |
|------------|------|---------------------------|
| [ryll](https://github.com/shakenfist/ryll) | SPICE client, and the home of the shared `shakenfist-spice-*` crates | `shakenfist-spice-protocol`: the link handshake and ticket authentication, which already implement the server role; message constants; the `logging::message_names` tables. `shakenfist-spice-compression`: image codecs, which today only decode. Ryll is also the reference client andris is tested against. |
| [kerbside](https://github.com/shakenfist/kerbside) | SPICE proxy | The constraints on what andris may say. Kerbside authenticates to a backend with SPICE ticket auth, offers clients spice-server's channel capability set, and terminates a session on any message type absent from ryll's `logging::message_names`. Its proxy already consumes `shakenfist-spice-protocol` for the server role. |

The boundaries that follow from this:

- **Wire types live in ryll, not here.** Ryll's display, cursor and
  main-channel message structures currently sit in
  `shakenfist-spice-renderer`, ryll's client substrate, and are
  parse-only. Andris must not depend on the renderer, so the types
  it needs are to move into `shakenfist-spice-protocol` and gain
  writers there, where kerbside can use them too.
- **Encoders live in ryll, not here.** An encoder belongs beside the
  decoder it round-trips against, in `shakenfist-spice-compression`.
- **Kerbside compatibility is by construction.** Because andris only
  emits what ryll models, a message andris can send is one kerbside
  already accepts.

The intended deployment is ryll connecting to kerbside, and kerbside
connecting to andris, which is attached to the X server of an
existing desktop session.

## Intended shape

None of this is built. It summarises the design commitments of
[`docs/plans/PLAN-x11-desktop.md`](docs/plans/PLAN-x11-desktop.md),
which is the authority; read the plan for the reasoning and the
order of work.

- **Attach, do not own.** Andris connects to a running X server as
  an ordinary client of the session user's display. It never starts
  or configures the desktop.
- **Pixels, not draw ops.** Capture is damage-driven pixel reads,
  sent as `DRAW_COPY` images and video streams, with no QXL draw-op
  emulation.
- **Platform seams.** Capture, input injection, display control,
  clipboard and audio sit behind traits, with X11 implementations
  first and a synthetic test-pattern frame source beside them, so
  that a Wayland backend can be added later without restructuring.
- **Ryll is the reference client.** Images are encoded as LZ4 or
  JPEG and streams as H.264; other SPICE clients work only where
  they support those encodings.
- **One client at a time.** A new connection replaces the old one.
- **Linux only.** The plan leaves the crate layout open, defaulting
  to a single binary crate with a module per backend.
