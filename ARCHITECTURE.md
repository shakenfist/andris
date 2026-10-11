# Architecture

This document is the map of andris: its components and how data
moves between them, how it relates to the repositories around it, and
the design commitments behind it. The documentation index is
[`docs/index.md`](docs/index.md); how to run andris is in
[`docs/running.md`](docs/running.md), and how to build, test and land
a change is in [`docs/development.md`](docs/development.md).

## Components

Andris is one crate, `andris`: a library with a thin binary
(`main.rs`) that parses the command line and logs to stderr. The
server (`andris/src/server/`) knows SPICE and nothing of X11; the
platform (`andris/src/platform/`) knows the desktop and nothing of
SPICE.

| Component | Where | What it does |
|-----------|-------|--------------|
| Configuration | `config.rs` | Command-line options and the checks across them: the TLS options go together, a plain listener must be loopback, the ticket file and TLS key must be private to andris's user, and the ticket at most 60 bytes |
| Listener and link | `server/mod.rs`, `link.rs`, `tls.rs` | One accept task per listener. A connection holds one of 8 handshake permits, and one of 4 slots for its source address, while it links and authenticates with the ticket, with a time limit on every step and a delay before a wrong ticket is refused. With TLS configured the plain listener only redirects clients to the TLS listener, under its own permits |
| Session registry | `server/session.rs` | Holds the one session served at a time. A main-channel link that authenticates begins a session and cancels the previous one; secondary channels must name the session's id. The session carries the mouse mode and a cancel signal every channel task races against, and holds one channel of each type: a new link of a type replaces the old channel |
| Channel framing | `server/channel.rs`, `opcodes.rs` | Mini-header message framing, the messages every channel shares, and the ack window. Each channel sends only variants of its own opcode enum |
| Main channel | `server/main_channel.rs` | `INIT`, the channel list, mouse mode and pings |
| Display channel | `server/display.rs`, `stats.rs` | The primary surface; the loop that turns damage into frames under the frame-rate cap and the ack window; the scrub; the lossless refresh of JPEG areas; the client's compression preference; and the per-session metrics |
| Updates | `server/updates/` | Keeps a shadow of what the client was last sent, compares each capture with it in 64x64 tiles, merges the changed tiles into at most 32 rectangles, splits any over 4 MiB, and encodes each from the shadow as JPEG (photographic areas, when the client leaves the choice to the server), LZ4 or an uncompressed bitmap |
| Inputs channel | `server/inputs.rs`, `keymap.rs` | Decodes set-1 scancodes to evdev codes, tracks lock keys, applies pointer events according to the mouse mode |
| Cursor channel | `server/cursor.rs` | Sends each cursor shape the platform reports as an alpha cursor |
| Platform traits | `platform/mod.rs`, `damage.rs` | `DisplayGeometry`, `ScreenSource`, `InputSink` and `CursorSource`, bundled as a `Platform`. They use no SPICE types, and keys cross them as Linux evdev codes. `ScreenSource` captures rectangles or the whole desktop, and hands each subscriber the damage reported since it last took some, as a region of 64x64 tiles |
| Synthetic platform | `platform/synthetic.rs` | A deterministic test pattern that reports damage as it moves, an input sink that records events, and one cursor shape; for tests, load testing and `--platform synthetic` |
| X11 platform | `platform/x11/` | XDamage on the root window, MIT-SHM capture through a shared memfd with `GetImage` as the fallback, XTEST input, XFIXES cursor and XKB key names, each on its own connection and thread; the MIT-SHM mapping is andris's only `unsafe` code (`platform/x11/shm.rs`; the rest of the crate denies it), because mapping memory that other processes write into cannot be expressed in safe Rust; it is kept sound by never making a Rust reference into the mapping and copying pixels out through raw pointers, so a concurrent writer can only tear pixels; the input queue is bounded, with room kept for releases; refuses an X server it cannot read as BGRX |

Around the code: `Makefile` and `.devcontainer/` run cargo in a
container; `scripts/e2e.sh` drives the end-to-end test;
`.github/workflows/` holds the two-tier CI; `docs/plans/` holds the
plans; `AGENTS.md` carries conventions for AI coding assistants;
`PLAN-TEMPLATE.md` and `PUSH-AUDIT.md` are the planning template and
the pre-push audit runbook.

### How a frame reaches the wire

1. **Damage.** The X11 platform subscribes to XDamage on the root
   window. Each subscriber (the display channel is one) is handed the
   damage reported since it last took some, as a region of 64x64
   tiles. Damage that arrives while a frame cannot start merges into
   the pending region; pixels are never queued.
2. **When to capture.** The display channel starts a frame only when
   damage is pending, at least `--frame-interval-ms` has passed since
   the last frame, and the ack window has room. A still desktop
   captures no frames.
3. **Capture.** It captures the region's rectangles through
   `ScreenSource`, on a blocking thread, as BGRX: with `ShmGetImage`
   into a shared memfd when MIT-SHM 1.2 is available, otherwise with
   `GetImage`.
4. **Shadow comparison.** The update encoder compares the capture,
   one tile at a time, with its shadow of what the client was last
   sent, and copies the changed tiles into the shadow.
5. **Rectangles and encoding.** It merges the changed tiles into at
   most 32 rectangles. Each becomes one `DRAW_COPY` drawn from the
   shadow, as JPEG when it looks photographic and the client leaves
   compression to the server, otherwise as LZ4, or as an uncompressed
   bitmap when LZ4 would not help or the client asked for none.
6. **Frame end.** A `MARK` ends the frame. The ack window counts
   messages and holds back the next frame while the client is too far
   behind; changes merge meanwhile, and one frame then catches it up.

Two background jobs feed the same loop. The scrub re-reads the desktop
a slice at a time, compares it with the shadow, and adds any
difference to the pending damage, catching changes the X server did
not report. The lossless refresh resends, as a frame of its own, tiles
that went out as JPEG once they have been still for a while, so a still
screen ends up exact. A capture of a new size re-creates the surface
first. [`docs/running.md`](docs/running.md) describes what an operator
sees of all this.

### How a key reaches the X server

The inputs channel receives set-1 scancode bytes, and its `KeyDecoder`
turns them into evdev key codes, handling the `E0` and `E1` prefixes.
They cross the platform seam as `InputEvent`s to the `InputSink`, which
only queues them. The X11 backend's input thread maps each evdev code to
an XKB key name through a table generated from xkeyboard-config, finds
the keycode the attached X server uses for that name (read once with
XKB `GetNames` at startup), and sends the press or release to the
server with XTEST. Using names rather than numbers is what makes keys
land correctly whichever keycode set the server was configured with.

## Relationship to ryll and kerbside

Andris is the third member of a SPICE trio in the shakenfist
organisation, and most of its protocol knowledge is meant to come
from the other two rather than be written again here.

| Repository | Role | What andris takes from it |
|------------|------|---------------------------|
| [ryll](https://github.com/shakenfist/ryll) | SPICE client, and the home of the shared `shakenfist-spice-*` crates | `shakenfist-spice-protocol`: the link handshake and ticket authentication, which already implement the server role; message constants; the `logging::message_names` tables. `shakenfist-spice-compression`: the LZ4 image encoder. Ryll is also the reference client andris is tested against, by `make e2e`. |
| [kerbside](https://github.com/shakenfist/kerbside) | SPICE proxy | The constraints on what andris may say. Kerbside authenticates to a backend with SPICE ticket auth, offers clients spice-server's channel capability set, and terminates a session on any message type absent from ryll's `logging::message_names`. Its proxy already consumes `shakenfist-spice-protocol` for the server role. |

The boundaries that follow from this:

- **Wire types live in ryll, not here.** Andris must not depend on
  `shakenfist-spice-renderer`, ryll's client substrate, so the message
  structures it needs live in `shakenfist-spice-protocol`, with
  writers, where kerbside can use them too.
- **Encoders live in ryll, not here.** An encoder belongs beside the
  decoder it round-trips against, in `shakenfist-spice-compression`.
- **Kerbside compatibility is by construction.** Because andris only
  emits what ryll models, a message andris can send is one kerbside
  already accepts.

The intended deployment is ryll connecting to kerbside, and kerbside
connecting to andris, which is attached to the X server of an
existing desktop session. Kerbside's static console source can front
andris today; [`docs/running.md`](docs/running.md) shows how. Ryll
also connects to andris directly.

Andris pins ryll's crates to one revision in `Cargo.toml`, and
`make ryll-headless` builds the client for the end-to-end test at the
same revision.

## Design commitments

These are the commitments of
[`docs/plans/PLAN-x11-desktop.md`](docs/plans/PLAN-x11-desktop.md),
which is the authority for the reasoning and for what is still to be
built.

- **Attach, do not own.** Andris connects to a running X server as
  an ordinary client of the session user's display. It never starts
  or configures the desktop.
- **Pixels, not draw ops.** Capture is pixel reads, sent as
  `DRAW_COPY` images, with no QXL draw-op emulation. Damage says
  where to look, and comparing with what the client was last sent
  says what to send. Video streams are yet to come.
- **Platform seams.** Capture, input injection and the cursor sit
  behind traits, with an X11 implementation and a synthetic one, so
  that a Wayland backend can be added later without restructuring.
  Display control, clipboard and audio are yet to be added.
- **Ryll is the reference client.** Images are LZ4 or uncompressed;
  other SPICE clients work only where they support those encodings.
- **One client at a time.** A new connection replaces the old one.
- **Linux only.** A single binary crate with a module per backend.
