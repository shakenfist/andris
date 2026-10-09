# Serve an existing X11 desktop over SPICE

## Prompt

Before responding to questions or discussion points in this
document, explore the andris repository and the ryll crates it
builds on. Read the relevant source and ground your answers in
what the code actually does today. Do not speculate about either
codebase when you could read it instead. Flag any uncertainty
explicitly rather than guessing.

Andris is a SPICE *server*. Ryll (`shakenfist/ryll`) is the SPICE
client whose shared crates andris depends on, and kerbside
(`shakenfist/kerbside`) is the SPICE proxy that sits between them.
Key references:

- `shakenfist-spice-protocol/src/link.rs` in ryll: the link
  handshake and ticket authentication for both roles. The server
  role is already exercised in production by kerbside-proxy.
- `kerbside/rust/kerbside-proxy/src/caps.rs` and `allowlist.rs`:
  the capabilities kerbside offers clients, and the per-channel
  message-type allowlist it enforces on everything andris sends.
- `/srv/src-reference/spice/spice/` (spice-server, LGPL) and
  `/srv/src-reference/spice/x11spice/` (GPLv3): references for
  *behaviour only*. Andris is Apache-2.0, like ryll; no code is
  translated from either.
- `/srv/src-reference/spice/spice-protocol/`: canonical wire
  definitions.

## Situation

The operator's daily development connection, from a macOS laptop
to a Linux development host, is RDP: xrdp 0.10.1 with xorgxrdp, whose
session is an ordinary Xorg server on display `:10` (2212x1310,
24-bit) owned by the operator. `sesman.ini` sets
`KillDisconnected=false` and `DisconnectedTimeLimit=0`, so that
session survives RDP disconnects indefinitely. Display `:10`
offers every X extension a pixel-capturing server needs:
DAMAGE, MIT-SHM, XFIXES, XTEST, RANDR and Composite.

The goal is to replace that connection with ryll, through
kerbside, for dogfooding. There is no good SPICE server to put on
the far end:

| Candidate | State (commit history checked 2026-10-08) | Problem |
|-----------|-------------------------------------------|---------|
| x11spice | Last substantive commit 2022-09; one commit since 2023; not packaged in Debian | Abandoned. GPLv3 C over libspice-server. |
| Xspice (`xf86-video-qxl`) | 12 commits since 2023, all janitorial; Debian 13 ships `xserver-xspice 0.1.6` | Is its own X server, so cannot share the xrdp session; built around QXL draw-op passthrough that modern toolkits do not exercise. |
| libspice-server | Alive (74 commits since 2023; 0.15.2 on the development host) | A library, not a desktop server; driving it from Rust means emulating a QXL device over FFI. |
| GNOME Remote Desktop, KRdp | Alive | RDP and VNC only. |

Ryll already holds much of a server:

- **Link and auth, server role.** `read_link_mess`,
  `send_link_reply`, `generate_ticket_keypair`, `read_auth_ticket`
  / `decrypt_password` and `send_auth_result` in
  `shakenfist-spice-protocol/src/link.rs`.
- **Wire knowledge, but on the wrong side of a crate boundary.**
  The display, cursor and main-channel (including vdagent) message
  structures are parse-only and live in
  `shakenfist-spice-renderer` (`channels/display.rs`,
  `channels/main_channel.rs`), the client substrate. A server must
  not depend on the renderer, so these structures have to move
  into `shakenfist-spice-protocol` and gain writers.
- **Compression is decode-only.** `shakenfist-spice-compression`
  decodes LZ, GLZ, LZ4, QUIC and JPEG and encodes nothing.
- **Distribution.** The shared crates are on crates.io at 0.1.7;
  kerbside-proxy consumes `shakenfist-spice-protocol` at a pinned
  ryll git revision. Andris can do either.

Kerbside constrains what andris may send, and that is a useful
discipline rather than a burden:

- It requires `MINI_HEADER` and `AUTH_SELECTION` from clients and
  authenticates to the backend with SPICE ticket auth only
  (`AUTH_SPICE`), so andris must implement ticket auth and the
  mini header.
- It offers clients spice-server's channel capability set
  (display: `MONITORS_CONFIG`, `PREF_COMPRESSION`,
  `PREF_VIDEO_CODEC_TYPE`, `STREAM_REPORT`; inputs:
  `KEY_SCANCODE`; playback: `VOLUME`, `OPUS`) and only *warns*
  when a backend lacks one. Andris should still advertise the
  same set, and accept the matching client messages even where it
  ignores them.
- Its L1 allowlist terminates a session on any message type that
  ryll's `logging::message_names` does not name. **Andris only
  ever emits message types ryll models** — an invariant that keeps
  andris kerbside-compatible by construction.

## Mission and problem statement

Build andris: a Rust SPICE server that exports an *existing* X11
display, so that the xrdp session on the development host can be
reached from ryll on macOS, through kerbside, while RDP continues to
work against the same desktop during the transition. The bar is
daily-driver use, not a demo: responsive typing and scrolling,
window resizing, clipboard, video that plays, and audio.

Design commitments:

- **Attach, do not own.** Andris connects to a running X server as
  an ordinary client (the session user's `DISPLAY` and
  `XAUTHORITY`). It never starts or configures the desktop. Xvfb
  appears only in CI.
- **Pixels, not draw ops.** Capture is damage-driven pixel reads;
  output is `DRAW_COPY` images and video streams. No QXL draw-op
  emulation.
- **Platform seams from day one.** Capture, input injection,
  display control (resize), clipboard and audio sit behind traits.
  X11 implementations (via `x11rb`) ship first. A synthetic
  test-pattern frame source ships with them, for CI and for
  kerbside load testing (the "custom SPICE server" that ryll's
  `docs/index.md` anticipates). A Wayland backend (PipeWire screen
  capture plus libei input) is future work that the seams must
  admit without restructuring.
- **Ryll is the reference client.** Encodings are LZ4 and JPEG for
  images and H.264 for streams; there is no GLZ or QUIC encoder.
  remote-viewer works only where it supports those encodings, as a
  best effort.
- **One client at a time.** A new connection replaces the old one,
  as spice-server does.

Not covered: Wayland, multiple simultaneous viewers, migration,
the record, usbredir, webdav and smartcard channels,
multi-monitor, and splitting ryll's shared crates into their own
repository (see Future work for the trigger).

## Open questions

Each has the default this plan takes if nobody answers.

1. **Who owns the desktop size when RDP and SPICE share `:10`?**
   xorgxrdp resizes the screen through RandR when the RDP client
   resizes. Unknown: whether it tolerates another X client
   changing RandR state while RDP is connected. *Default:* the
   most recent client to ask wins; phase 7 opens with a spike that
   settles whether that is safe.
2. **HiDPI.** Does ryll on a Retina Mac request its window size in
   physical or logical pixels through `VD_AGENT_MONITORS_CONFIG`?
   *Default:* physical pixels, with desktop scaling left to the
   session's own DPI settings. Settled in phase 7.
3. **Exposure.** *Default:* andris binds to a configured address,
   loopback unless told otherwise. It reads its ticket from a
   `0600` file and refuses to listen on a non-loopback address
   without TLS. Kerbside's static source fronts it
   (`hypervisor_ip`, `insecure_port`/`secure_port`, `ticket`).
4. **Where encoders live.** *Default:* in
   `shakenfist-spice-compression` behind a non-default `encode`
   feature, so the encoder can be round-trip tested against the
   decoder and ryll's client builds do not pull encoder
   dependencies.
5. **Crate layout in andris.** *Default:* one binary crate with
   modules per backend, split only when a second backend exists.
6. **Packaging.** *Default:* Linux only. A `.deb` via `cargo deb`
   and a systemd *user* unit, built inside Docker like ryll. No
   macOS, Windows or Homebrew builds.

## Execution

Phases 2 and 3 land in ryll; the rest land here. A phase that
lands in ryll records `ryll <sha> (#pr)` in `Merged` and is audited
in ryll as part of the pull request that lands it. Phase plans are
written with `/next-phase` as each phase comes up.

| Phase | Plan | Status | Merged |
|-------|------|--------|--------|
| 0. Join the consistency audit | PLAN-x11-desktop-phase-00-audit.md | Complete | andris `60c0c4d..e89539f`; development `066d639` |
| 1. Build and CI scaffold | PLAN-x11-desktop-phase-01-scaffold.md | Complete | andris `cf34d91` (#1) |
| 2. Server-role wire types (ryll) | Not yet written | Not started | |
| 3. Image encoders (ryll) | Not yet written | Not started | |
| 4. Server skeleton | Not yet written | Not started | |
| 5. Damage and flow control | Not yet written | Not started | |
| 6. Sharing the xrdp session | Not yet written | Not started | |
| 7. In-process agent: resize and clipboard | Not yet written | Not started | |
| 8. Video streams | Not yet written | Not started | |
| 9. Audio | Not yet written | Not started | |
| 10. Packaging and dogfood soak | Not yet written | Not started | |
| 11. Push audit | This file, below | Not started | |

Rough sizing, as judgement rather than measurement: phases 0–5
reach a usable LAN desktop in about three to four weeks; phases 6–9
reach daily-driver quality in a further five to seven.

### Phase 0: Join the consistency audit

Before there is any code, bring andris into shakenfist/development's
consistency-audit ecosystem, so that the fleet's conventions answer
the structural questions rather than this plan guessing at them.
Detail is in
[PLAN-x11-desktop-phase-00-audit.md](PLAN-x11-desktop-phase-00-audit.md).
In outline:

- Create `shakenfist/andris` on GitHub with `develop` as its default
  branch, and push this plan as the first commit.
- Run `scripts/audit-check.py` from development against the clone,
  and seed whatever its failures ask for from development's
  templates and shared blocks, using ryll's copies as the nearest
  Rust example.
- Add andris to development's audit matrix and in-scope list, once
  the GitHub repository exists so the daily run can clone it.
- **Human review is out of scope for now.** The review-coverage and
  review-scope-completeness checks apply only to a repository that
  carries `.vscode/review-scope.toml`. Andris does not get the
  review-tracking tooling, so they report not applicable and
  development needs no override.

Some checks cannot pass until there is code and CI: CodeQL, the CI
review automation, and the merge queue, whose required status
checks come from CI. Phase 1 closes those.

### Phase 1: Build and CI scaffold

Detail is in
[PLAN-x11-desktop-phase-01-scaffold.md](PLAN-x11-desktop-phase-01-scaffold.md).
In outline:

- a Cargo workspace with one zero-dependency stub binary and the
  fleet's unwrap lint;
- a `Makefile` that runs cargo offline inside a Docker devcontainer,
  mirroring ryll's;
- the Rust pre-commit hook, joining the hooks phase 0 added;
- `deny.toml` and a weekly supply-chain workflow;
- two-stage CI with gates and the automated reviewer;
- the work phase 0 deferred: CodeQL, the re-review and retest
  workflows, and the merge queue with its required checks.

The phase is done when the local audit run reports no failures. The
human-review checks report not applicable rather than failing, so
nothing is exempted.

### Phase 2: Server-role wire types (ryll)

Move the display, cursor, inputs, main and vdagent message
structures that andris needs out of `shakenfist-spice-renderer` into
`shakenfist-spice-protocol`, each with a reader and a writer, and
re-point the renderer at them. The set to move:

- **main:** `INIT`, `CHANNELS_LIST`, `MOUSE_MODE`, the `AGENT_*`
  family, and the vdagent messages for monitors config and
  clipboard;
- **display:** `SURFACE_CREATE`/`DESTROY`, `MARK`, `RESET`,
  `DRAW_COPY` with its image descriptor, `MONITORS_CONFIG`, the
  `STREAM_*` family, and the client's `INIT`, `STREAM_REPORT` and
  preference messages;
- **cursor:** `INIT`, `SET`, `MOVE`, `HIDE`;
- **inputs:** both directions.

Every moved structure gets a round-trip test (write, then parse
with the same code ryll's client uses). This is a refactor of
ryll's client and must not change its behaviour; kerbside's
allowlist tests must still pass against the new revision. It is the
riskiest phase for ryll, so plan it at high effort.

### Phase 3: Image encoders (ryll)

Add SPICE-framed LZ4 and JPEG image encoders to
`shakenfist-spice-compression` behind an `encode` feature. Round-trip
each against the existing decoders, and add property tests over
random sizes and strides. Release ryll so andris can depend on
crates.io versions; until then, andris uses a Cargo `[patch]`
against a local ryll worktree.

### Phase 4: Server skeleton

The first usable server, as one pull request built up in steps:

- a TCP listener with optional rustls TLS;
- the link handshake and ticket auth, advertising the capability
  sets listed under Situation;
- the main channel (`INIT`, channel list, mouse mode);
- a display channel with one primary surface the size of the root
  window, sending full-frame LZ4 `DRAW_COPY` updates on a timer;
- an inputs channel that maps PC scancodes to X keycodes and
  injects keys and pointer events through XTest;
- a cursor channel driven by XFixes cursor notifications.

The capture, input and display-control traits are defined here,
with the X11 and synthetic implementations. CI runs andris against
Xvfb and drives it with ryll's headless mode, asserting that frames
arrive and that injected keys reach an X client. A smoke test
confirms that kerbside's static source can front andris without an
allowlist termination.

### Phase 5: Damage and flow control

Replace the timer with XDamage-driven updates. Coalesce damage into
rectangles and send only those, honouring the client's `SET_ACK` /
`ACK` window. When the client falls behind, merge stale damage
instead of queueing it. Choose JPEG for photographic regions and
LZ4 for the rest, using a measured heuristic. Expose per-client
metrics (frame rate, bytes out, encode time, ack lag) and record
PING round-trips. This is the phase that makes andris pleasant
rather than merely working.

### Phase 6: Sharing the xrdp session

Run andris against `:10` with RDP connected at the same time. This
covers:

- a systemd user unit that finds the session's `DISPLAY` and
  `XAUTHORITY`;
- behaviour when the X server goes away (exit cleanly, let systemd
  restart);
- confirming that RDP and SPICE input interleave sanely.

Document the operator setup in `docs/`.

### Phase 7: In-process agent: resize and clipboard

SPICE clients send resize requests and clipboard traffic through
the vdagent protocol on the main channel. Andris is already inside
the session, so it plays the agent itself, with no separate
`spice-vdagent` process:

- announce an agent connection;
- map `VD_AGENT_MONITORS_CONFIG` to RandR;
- bridge `CLIPBOARD` and `PRIMARY` selections to and from the
  client, coexisting with `xrdp-chansrv`, which also owns
  selections.

The phase opens with the spike from open questions 1 and 2.

### Phase 8: Video streams

Detect rectangles with a sustained high update rate and serve them
as H.264 streams (`STREAM_CREATE`, `STREAM_DATA`), using openh264
first. Adapt bitrate and frame rate from the client's
`STREAM_REPORT`, and tear streams down when the region goes quiet.
Ryll's stream-flap diagnostics are the instrument for tuning this.

### Phase 9: Audio

Add a playback channel that captures the session's default sink
monitor (the xrdp sink on the development host), encodes Opus at 48
kHz and honours the client's volume and mute messages.

### Phase 10: Packaging and dogfood soak

Ship a `.deb` and the systemd user unit, and write the kerbside
static-source configuration into `docs/`. Then the operator uses
ryll → kerbside → andris as the daily connection for a week, with
RDP as the fallback. Every regression found is filed, and either
fixed here or recorded under Future work.

### Phase 11: Push audit

Run `PUSH-AUDIT.md` over the merge commits recorded in the
`Merged` column for the andris phases. For phases 2 and 3, cite the
audits run in ryll's pull requests rather than repeating them.

## Agent guidance

### Execution model

All implementation is done by sub-agents briefed from the phase
plans, never in the management session. The management session
plans, reviews the actual diffs and commits.

### Planning effort

This master plan was written at high effort. Phases 2, 4, 5, 7 and
8 turn on protocol semantics, concurrency or heuristics and should
be planned at high effort. Phases 0, 1, 3, 6, 9 and 10 follow
established patterns and can be planned at medium effort.

### Management session review checklist

- [ ] `make lint` and `make test` pass (cargo runs in Docker,
      never on the host).
- [ ] Andris emits no message type absent from ryll's
      `logging::message_names` tables.
- [ ] No code was translated from x11spice or spice-server.
- [ ] Ryll-side changes keep ryll's client behaviour unchanged,
      and its existing tests pass.

## Administration and logistics

### Success criteria

We will know when this plan has been successfully implemented
because the following statements will be true:

* The operator has used ryll → kerbside → andris as the daily
  connection to the development host for a week, with RDP still
  available against the same session.
* With a static screen, andris uses under 2% of one core on
  the development host.
* Scrolling a full-screen terminal at the session's native size
  sustains at least 30 frames per second on the LAN, measured by
  ryll's statistics.
* Resizing the ryll window resizes the desktop, and copy and
  paste work in both directions.
* A video played on the desktop arrives as an H.264 stream with
  audio.
* CI exercises andris end to end against Xvfb with ryll headless,
  and through kerbside.
* `docs/` describes running andris alongside xrdp, without
  reference to plan phases.
* The daily consistency audit runs against andris and files no
  issues, apart from the documented human-review exemption.

### Documentation index maintenance

Add this plan's row to `docs/plans/index.md` (done with this
file) and keep its status in step with the Execution table.

### Future work

- A Wayland backend: PipeWire screen capture via
  xdg-desktop-portal, and libei for input.
- Splitting ryll's shared crates (protocol, compression, usbredir)
  into their own repository. The trigger is phase 3 merging: at
  that point there are three real consumers (ryll, kerbside,
  andris) and the shared surface is known.
- VA-API H.264 encode; multi-monitor; the record channel
  (microphone); usbredir and webdav; several simultaneous viewers.
- Kerbside L2 inspection reusing the phase 2 wire types.
- A side-by-side comparison against Xspice as a baseline.

### Bugs fixed during this work

None yet.

### Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.
