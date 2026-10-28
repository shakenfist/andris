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
- **Wire knowledge, mostly without types.** The protocol crate's
  `messages.rs` has read-only types for a handful of server
  messages (`MainInit`, `ChannelsList`, `SurfaceCreate`, `DrawBase`,
  `ImageDescriptor`, `CursorInit`, `CursorSet`) and write-only ones
  for the client's inputs messages. Everything else, including
  the vdagent messages, streams and the DRAW_COPY body, is parsed
  inline at fixed offsets in `shakenfist-spice-renderer`'s channel
  code. A server must not depend on the renderer, so these
  messages need protocol-crate types with readers and writers.
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
   *Answered by phase 7's survey:* RDP does. xorgxrdp 0.10.2 refuses
   every RandR screen-size change an X client asks for, whether or
   not RDP is connected; only its own resizes, made when the RDP
   client resizes, are allowed. *Decided by the operator on
   2026-10-14:* andris resizes through RandR where the X server
   allows it, and on xorgxrdp says no and leaves the size alone.
2. **HiDPI.** *Answered by phase 7's survey:* ryll asks in egui
   points, about half the window's pixel size on a Retina Mac
   (ryll#485). That is ryll's to change, and it does not matter on
   xrdp, where no SPICE resize applies.
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
6. **Packaging.** *Decided by the operator on 2026-10-13:* Linux
   only, published to PyPI as manylinux wheels, the way ryll is
   published, because most people will install a wheel. A `.deb` may
   be built as well, but is not the primary channel. No macOS,
   Windows or Homebrew builds.
7. **H.264 patents in what andris ships.** openh264's code is
   BSD-2-Clause, but Cisco's patent coverage applies only to Cisco's
   own binary, downloaded separately onto the user's machine; a build
   from source carries none. *Decided by the operator on 2026-10-15:*
   phase 8 builds openh264 from source, as ryll already ships it, and
   documents the position. *Decided by the operator on 2026-10-18,*
   for phase 10: the PyPI wheels ship the compiled encoder, as ryll's
   do, rather than loading a Cisco binary the operator fetches.

## Execution

Phases 2 and 3 land in ryll; the rest land here. A phase that
lands in ryll records `ryll <sha> (#pr)` in `Merged` and is audited
in ryll as part of the pull request that lands it. Phase plans are
written with `/next-phase` as each phase comes up.

| Phase | Plan | Status | Merged |
|-------|------|--------|--------|
| 0. Join the consistency audit | PLAN-x11-desktop-phase-00-audit.md | Complete | andris `60c0c4d..e89539f`; development `066d639` |
| 1. Build and CI scaffold | PLAN-x11-desktop-phase-01-scaffold.md | Complete | andris `cf34d91` (#1) |
| 2. Server-role wire types (ryll) | PLAN-x11-desktop-phase-02-wire-types.md | Complete | ryll `3763158` (#477); andris `a9e79cf` (#5) |
| 3. Image encoders (ryll) | PLAN-x11-desktop-phase-03-encoders.md | Complete | ryll `cd2fe41` (#487); andris `591e062` (#6) |
| 4. Server skeleton | PLAN-x11-desktop-phase-04-server-skeleton.md | Complete | andris `1950cdb` (#7) |
| 5. Damage and flow control | PLAN-x11-desktop-phase-05-damage.md | Complete | andris `68b1058` (#8); andris `43bfe88` (#11) |
| 6. Sharing the xrdp session | PLAN-x11-desktop-phase-06-sharing.md | Complete | andris `498a72c` (#12) |
| 7. In-process agent: resize and clipboard | PLAN-x11-desktop-phase-07-agent.md | Complete | ryll `b298b39` (#534); andris `b7ba2dd` (#16) |
| 8. Video streams | PLAN-x11-desktop-phase-08-video.md | Complete | ryll `a157ced` (#552), `601b9dc` (#554), `49072fc` (#557); andris `c2fe8a7` (#21), `1ecafa5` (#23) |
| 9. Audio | PLAN-x11-desktop-phase-09-audio.md | Complete | ryll `38c51ef` (#578); andris `d8f316a` (#27) |
| 10. Packaging and dogfood soak | PLAN-x11-desktop-phase-10-packaging.md | Complete | andris `553ecfb` (#28), `c0617e9` (#29), `c36e87a` (#49) |
| 11. Human review | PLAN-x11-desktop-phase-11-review.md | In progress | |
| 12. Push audit | This file, below | Not started | |

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
  development needs no override. Phase 11 brings it in.

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

Detail is in
[PLAN-x11-desktop-phase-02-wire-types.md](PLAN-x11-desktop-phase-02-wire-types.md).
Give the display, cursor, inputs, main and vdagent messages that
andris needs types in `shakenfist-spice-protocol`, each with a
reader and a writer, and re-point the renderer's inline parsing at
them. Most of these types do not exist yet. The renderer parses the
wire inline rather than through structs of its own, so this phase
mostly creates types rather than moving them. The set:

- **main:** `INIT`, `CHANNELS_LIST`, `MOUSE_MODE`, the `AGENT_*`
  family, and the vdagent messages for monitors config and
  clipboard;
- **display:** `SURFACE_CREATE`/`DESTROY`, `MARK`, `RESET`,
  `DRAW_COPY` with its image descriptor, `MONITORS_CONFIG`, the
  `STREAM_*` family, and the client's `INIT`, `STREAM_REPORT` and
  preference messages;
- **cursor:** `INIT`, `SET`, `MOVE`, `HIDE`;
- **inputs:** both directions.

Every type gets a round-trip test (write, then parse with the
same code ryll's client uses). This is a refactor of
ryll's client and must not change its behaviour; kerbside's
allowlist tests must still pass against the new revision. It is the
riskiest phase for ryll, so plan it at high effort.

### Phase 3: Image encoders (ryll)

Detail is in
[PLAN-x11-desktop-phase-03-encoders.md](PLAN-x11-desktop-phase-03-encoders.md).
Add SPICE-framed LZ4 and JPEG image encoders to
`shakenfist-spice-compression` behind an `encode` feature. Round-trip
each against ryll's decoders, and add property tests over random
sizes and strides.

Ryll's LZ4 decoder does not match spice-server, as phase 2's survey
found. Ryll reads no `data_size` prefix, decodes independent per-row
blocks rather than one LZ4 stream over multi-line chunks, and maps
bitmap format bytes differently from `enums.h`. A round trip against
that decoder would prove nothing. So this phase opens by settling
the LZ4 framing against a capture from spice-server and fixing
ryll's decoder to match (ryll#475), before it writes the encoder.
Ryll has never received LZ4: spice-server sends it only to a client
that asks for it, and ryll asks for `AUTO_GLZ`. So the capture needs
a ryll option to ask for LZ4.

This phase does not release ryll. Ryll 0.2.0 is held until this
phase lands, so that one release carries the breaking changes of
phases 2 and 3; cutting it is not a step of this plan. From phase 4, andris depends
on ryll's crates by git revision, as kerbside does, which andris's
`deny.toml` already allows. A path `[patch]` against a sibling ryll
worktree would not work, because andris's build container mounts
only the andris checkout. Andris could move to crates.io versions
once a release contains what it needs. Phase 10's survey found that
nothing forces it: a wheel embeds the compiled binary, so a git
dependency costs it nothing, and only publishing andris itself to
crates.io would rule one out. Phase 10 keeps the git pin.

### Phase 4: Server skeleton

Detail is in
[PLAN-x11-desktop-phase-04-server-skeleton.md](PLAN-x11-desktop-phase-04-server-skeleton.md).
The first usable server, as one pull request built up in steps:

- a TCP listener with optional rustls TLS;
- the link handshake and ticket auth, advertising the capability
  sets listed under Situation;
- the main channel (`INIT`, channel list, mouse mode);
- a display channel with one primary surface the size of the root
  window, sending LZ4 `DRAW_COPY` updates on a timer, in bands, only
  for the bands that changed;
- an inputs channel that maps PC scancodes to X keycodes and
  injects keys and pointer events through XTest. The keycodes are
  looked up by XKB key name from the attached server, because the
  xrdp session `:10` uses xfree86 keycodes and Xvfb uses evdev;
- a cursor channel driven by XFixes cursor notifications.

The capture, input and display-control traits are defined here,
with the X11 and synthetic implementations. CI runs andris against
Xvfb and drives it with ryll's headless mode, built at the revision
andris pins, through ryll's control socket. It asserts that frames
arrive and that injected keys reach an X client. Released ryll is
too old for this. A smoke test, run by hand in this phase, confirms
that kerbside's static source can front andris without an allowlist
termination. A static source needs MariaDB and kerbside's API,
daemon and proxy, so automating that check is left to phase 10.

Ryll only asks for the channel list after the main channel's first
`PING` (ryll#502), so andris pings straight after `INIT`.

Andris depends on ryll's protocol and compression crates by git
revision (see phase 3). It sends LZ4 only to a client that
advertises `SPICE_DISPLAY_CAP_LZ4_COMPRESSION`, and an uncompressed
`BITMAP` otherwise, or when LZ4 would be larger than the raw pixels.
It sets each image descriptor's width and height to the bitmap's
exactly, and sends no `CACHE_ME` flag until it tracks the client's
pixmap cache. Its `deny.toml` needs an IJG exception for
`jpeg-encoder`.

Phase 2's readers check a message's structure, not its meaning, so
andris must validate what clients send before acting on it:
- the display `INIT` cache sizes, which can be negative or huge;
- monitor sizes and positions, where `w * h * 4` can overflow;
- the pointer's display id and coordinates;
- agent token counts, using saturating arithmetic;
- codec preferences and `STREAM_REPORT` fields.

### Phase 5: Damage and flow control

Detail is in
[PLAN-x11-desktop-phase-05-damage.md](PLAN-x11-desktop-phase-05-damage.md).

- **Damage drives updates.** XDamage on the root says where to look,
  and a comparison against the pixels last sent says what changed.
  XDamage over-reports on `:10`, so its rectangles alone would send
  far too much.
- **The frame timer stays as a cap** on the frame rate, not a polling
  period.
- **Capture uses MIT-SHM**, falling back to `GetImage`.
- **A slow scrub** of the whole screen catches damage XDamage missed.
- **Changed tiles go out as merged rectangles**, honouring the
  client's `SET_ACK` / `ACK` window. When the client falls behind,
  stale damage is merged rather than queued.
- **A `MARK` after each frame**, because ryll's FPS is the rate of
  `MARK` messages.
- **JPEG for photographic regions** and LZ4 for the rest, using a
  measured heuristic. A lossy region is refreshed losslessly once it
  is still.
- **Per-client metrics:** frame rate, bytes out, capture and encode
  time, ack lag, and the ping round trip, which andris has logged at
  debug since phase 4.

Headless ryll has no frame rate or byte counts that a test can read
(ryll#512). So the 30 fps criterion is measured with ryll's GUI.

This is the phase that makes andris pleasant rather than merely
working.

### Phase 6: Sharing the xrdp session

Detail is in
[PLAN-x11-desktop-phase-06-sharing.md](PLAN-x11-desktop-phase-06-sharing.md).
Run andris against an xrdp session with RDP connected at the same
time:

- **A containerised xrdp test bed** (`make e2e-xrdp`): a real xrdp,
  xorgxrdp and FreeRDP client in a container. Interleaving, resizes,
  reconnects and logout are tested there, never on the operator's
  live `:10`.
- **Interleaving checks** for keys, modifiers, lock keys, an RDP
  resize, an RDP reconnect and a change of layout, with fixes for
  whatever they find.
- **Process lifetime.** Andris neither exited when the X server went
  away nor handled SIGTERM; it served until Ctrl-C. This phase makes
  it exit non-zero when the desktop goes, and stop cleanly on
  SIGTERM, so systemd can restart it.
- **A systemd user unit, `andris@.service`,** keyed by display
  number, with `XAUTHORITY` from the default. The manager's own
  `DISPLAY` arrives too late in an xrdp session to rely on.
- **The operator setup** in `docs/xrdp.md`, and an acceptance run on
  `:10` with the operator present.

### Phase 7: In-process agent: resize and clipboard

Detail is in
[PLAN-x11-desktop-phase-07-agent.md](PLAN-x11-desktop-phase-07-agent.md).
SPICE clients send resize requests and clipboard traffic through the
vdagent protocol on the main channel. Andris is already inside the
session, so it plays the agent itself, with no separate
`spice-vdagent` process:

- **A ryll pull request first.** Ryll's headless mode gains an
  in-memory clipboard and control-socket verbs to set and read it and
  to send a resize, so `make e2e` can drive the agent through real
  ryll. Andris then bumps its ryll pin, which it needs anyway: agent
  data spanning several `AGENT_DATA` messages is reassembled only
  since ryll#474 was fixed, after the current pin.
- **The agent protocol:** announced in `MAIN_INIT`, with token flow
  control both ways, chunking, reassembly capped in size, and the
  token-count validation phase 4 deferred.
- **Resize through RandR where the X server allows it.** xorgxrdp
  refuses every client resize, so on the xrdp session andris says no,
  once, and the desktop keeps following the RDP client's size.
- **Text on the CLIPBOARD selection, both ways**, coexisting with
  `xrdp-chansrv`, which bridges CLIPBOARD for RDP. Andris fetches
  eagerly and drops a text that comes back unchanged, so the two
  bridges cannot loop through a machine running both clients. Ryll
  carries only text on CLIPBOARD, so PRIMARY and images are Future
  work.

Phase 2's `ClipboardGrab` does not model the grab serial, so andris
does not announce `VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL`.

### Phase 8: Video streams

Detect areas of the desktop that keep changing and look
photographic, and serve each as an H.264 stream (`STREAM_CREATE`,
`STREAM_DATA`) encoded with openh264, built from source with nasm.
Adapt the bitrate from the client's `STREAM_REPORT` and from andris's
own flow control, and tear a stream down when its area goes quiet,
repairing the area losslessly. One stream per display channel.

The phase starts with a ryll pull request:
- ryll's H.264 encoder moves from its renderer crate, which andris
  may not use, into `shakenfist-spice-compression`;
- ryll advertises `CODEC_H264` by default while preferring MJPEG, so
  spice-server keeps choosing MJPEG (ryll#398);
- a stream-compositing stride bug is fixed;
- headless mode gains a `stats` verb (ryll#512), so e2e can assert on
  streams.

A client without `CODEC_H264` keeps getting JPEG draws. Ryll's
stream-flap diagnostics are the instrument for tuning this. Planned in
[PLAN-x11-desktop-phase-08-video.md](PLAN-x11-desktop-phase-08-video.md).

### Phase 9: Audio

Add a playback channel that captures the session's default sink
monitor over the PulseAudio protocol, which PipeWire's
`pipewire-pulse` also serves, and encodes Opus at 48 kHz in 10 ms
frames. The development host runs PipeWire with no xrdp sink module,
so its default sink is a local output; with an xrdp sink module
installed, andris captures that sink's monitor instead. `VOLUME` and
`MUTE` carry the desktop's settings from server to client (the client
cannot set them); the phase plan defers forwarding them. Ryll first
gains playback wire types in its protocol crate and a `playback`
section in its control socket's `stats`.

### Phase 10: Packaging and dogfood soak

Detail is in
[PLAN-x11-desktop-phase-10-packaging.md](PLAN-x11-desktop-phase-10-packaging.md).
Publish andris to PyPI as manylinux wheels, reusing ryll's release
tooling: maturin with `bindings = "bin"`, a manylinux_2_28 build per
architecture (ryll's `.devcontainer/wheel/Dockerfile` and
`tools/build-ryll-wheel.sh`), and trusted publishing from a release
workflow. Andris's X11 code is pure-Rust `x11rb`, but phase 9 made
andris link libpulse, which is outside the manylinux policy. So this
phase loads libpulse at run time instead, which earns the tag and
lets andris run without sound where libpulse is missing. The wheels
ship openh264 compiled in (open question 7). A `.deb` is left to
Future work.

A wheel cannot install a systemd unit, and phase 6's
`andris@.service` names a bare `andris`, which systemd resolves only
in `/usr/local/bin` and `/usr/bin`; a wheel puts the binary in
`~/.local/bin` or a virtual environment. So andris gains an
`andris install-unit` subcommand. It writes the unit, compiled into
the binary, to `~/.config/systemd/user/` with `ExecStart=` naming the
running binary's absolute path. It also creates `~/.config/andris/`
with mode 0700 and a fresh 0600 ticket when none exists, so that
`docs/xrdp.md`'s install section becomes one command and an
`enable`. Reinstalling the wheel elsewhere means running it again,
which the documentation says.

Phase 6 declined `SystemCallFilter=@system-service` until a soak
shows MIT-SHM and the memfd survive it; this phase adds it, and its
soak is that test.

The kerbside static-source configuration has been in `docs/` since
phase 4. This phase automates phase 4's kerbside smoke test as a
merge-tier lane in andris's CI, on kerbside's demo stack, covering
the video and playback channels that did not exist in phase 4. Then
andris 0.1.0 is released, and the operator uses
ryll → kerbside → andris as the daily connection for a week, with
RDP as the fallback, through kerbside's demo stack on the development
host. Every regression found is filed, and either fixed in an
acceptance pull request or recorded under Future work.

### Phase 11: Human review

Detail is in
[PLAN-x11-desktop-phase-11-review.md](PLAN-x11-desktop-phase-11-review.md).
Asked for by the operator on 2026-10-14, who wants to read andris and
learn how it works. Bring andris into shakenfist/development's
review-tracking tooling, as ryll is, by the steps of development's
"Adopting a repository":

- `.vscode/review-scope.toml`, modelled on ryll's: the Rust, tests,
  scripts, documentation, CI workflows, manifests, `deny.toml`, the
  `Makefile` and the systemd unit are in scope. Its `order = 'listed'`
  makes the tooling's `next` offer files in the reading guide's order;
- the template's `tools/review-tracking.sh`, `tools/ci-prune-reviews.sh`
  and `prune-reviews` workflow, byte for byte, and a generated
  `REVIEWS.md`;
- a `.gitignore` that admits the review state, pre-commit excludes for
  the review marks, CI that skips review-only changes in both tiers,
  and a `DEPENDENCIES_TOKEN` secret, since andris's ruleset requires
  pull requests;
- five audits then apply: review-coverage, review-scope-completeness,
  review-tracking-templates, review-marks-pre-commit and
  review-scope-ci. Review-coverage keeps an issue open while five or
  more in-scope files need review, which it will for most of the
  phase.

The phase also writes a reading guide in `docs/`: an order to read the
code in, from the binary's entry point through the server, the
channels and the platform backends, saying for each part what it does
and what to look at. The operator then reviews the files in review
sessions, each landing as a pull request. Findings land as their own
pull requests. The phase is done when every in-scope file is reviewed
on one commit of `develop`.

### Phase 12: Push audit

Run `PUSH-AUDIT.md` over the merge commits recorded in the
`Merged` column for the andris phases. For phases 2 and 3, cite the
audits run in ryll's pull requests rather than repeating them.

## Agent guidance

### Execution model

All implementation is done by sub-agents briefed from the phase
plans, never in the management session. The management session
plans, reviews the actual diffs and commits.

### Planning effort

This master plan was written at high effort. Phases 2, 3, 4, 5, 7
and 8 turn on protocol semantics, concurrency or heuristics and
should be planned at high effort; phase 3 joined them when its
survey found the LZ4 decoder had to be rewritten first. Phases 0, 1,
6, 9, 10 and 11 follow established patterns and can be planned at
medium effort.

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
  available against the same session. *Partly met in phase 10:* the
  week's soak (2026-10-19 to 10-27) used ryll → andris through an
  SSH tunnel, with RDP available and never needed. Kerbside is
  exercised only by the `Through kerbside` merge-tier lane.
* With a static screen, andris uses under 2% of one core on
  the development host.
* Scrolling a full-screen terminal at the session's native size
  sustains at least 30 frames per second on the LAN, as ryll's GUI
  reports it: its FPS is the rate of `MARK` messages.
* Copy and paste of text work in both directions, with RDP
  connected to the same session. Resizing the ryll window resizes
  the desktop on an X server that allows it; on xrdp the desktop
  follows the RDP client's size, and andris says so.
* Every file in andris's review scope carries a current human
  review mark.
* A video played on the desktop arrives as an H.264 stream with
  audio. *Met in phase 10:* step 7's check, and 33 streams with sound
  during the soak.
* CI exercises andris end to end against Xvfb with ryll headless,
  and through kerbside.
* `docs/` describes running andris alongside xrdp, without
  reference to plan phases.
* The daily consistency audit runs against andris and files no
  issues. Until phase 11's review is done, review-coverage keeps one
  open; the audit has no exemption mechanism, and that issue is the
  review's work queue.

### Documentation index maintenance

Add this plan's row to `docs/plans/index.md` (done with this
file) and keep its status in step with the Execution table.

### Future work

- A Wayland backend: PipeWire screen capture via
  xdg-desktop-portal, and libei for input.
- Splitting ryll's shared crates (protocol, compression, usbredir)
  into their own repository. The trigger is phase 4 merging: at
  that point andris is the third real consumer, beside ryll and
  kerbside, and the shared surface is known.
- Several simultaneous video streams on one display channel. Phase 8
  allows one, the largest qualifying area, which suits one video on a
  desktop and keeps rate control and teardown simple.
- Whole-screen H.264, as xrdp and gnome-remote-desktop do. It is a
  different design from streaming photographic areas, and blurs text,
  which SPICE avoids.
- Video streams for clients without H.264, as MJPEG. An MJPEG stream
  would cost about what JPEG draws do now, so phase 8 left it out.
- Growing a video stream that covers only part of a video. Phase 8's
  operator acceptance saw a stream hold the lower half of a resumed
  video for 23 s, likely because the whole video is 1.83 times its
  area and a stream is replaced only by a cluster more than twice its
  size; the upper half went as JPEG at up to 38 Mbit/s meanwhile. A
  cluster that contains the stream could replace it whatever its size.
- Forwarding the desktop's volume and mute to clients. Andris
  advertises the playback `VOLUME` capability but sends neither
  `VOLUME` nor `MUTE`. Following the sink's volume needs libpulse's
  asynchronous API, and a monitor that is taken after the sink's
  volume (a null sink's) would be attenuated twice, so it needs the
  sink's kind taken into account.
- Following a change of default sink mid-session. Andris keeps the
  monitor it opened until the client's playback channel reconnects.
- Handling the vdagent's `VD_AGENT_AUDIO_VOLUME_SYNC`, which is how a
  client's own volume reaches a guest. Andris's in-process agent
  ignores it.
- VA-API H.264 encode; multi-monitor; the record channel
  (microphone); usbredir and webdav; several simultaneous viewers.
- Kerbside L2 inspection reusing the phase 2 wire types.
- Move andris's set-1 scancode decoder into ryll's protocol crate,
  beside the encoder's tables, so that encode and decode can be tested
  against each other.
- A side-by-side comparison against Xspice as a baseline.
- Daily use through kerbside. Phase 10's soak ran ryll → andris
  through an SSH tunnel, so kerbside has carried andris only in the
  merge-tier lane's short sessions, never a working day.
- A `.deb` or `.rpm` of andris. Phase 10 ships PyPI wheels and
  `andris install-unit`, which cover the install story.
- Depending on ryll's crates by crates.io version rather than git
  revision, once a ryll release contains ryll#578 (0.3.0 does not).
  Only a crates.io release of andris would require it.
- Scroll detection: send a scrolled region as `COPY_BITS`, which ryll
  renders, plus the newly exposed strip, rather than as fresh pixels.
  Phase 5 measures whether scrolling needs it, and sizes it.
- Pointer position for server mouse mode: send `CURSOR_MOVE`, and the
  real position in `CURSOR_INIT` and `CURSOR_SET`, so that a client in
  server mode draws the pointer where the desktop has it. Phase 4
  advertises server mode and applies relative motion, but reports no
  position.
- Running phase 6's xrdp test bed (`make e2e-xrdp`) in CI. Phase 6
  keeps it local, because it guards xrdp's behaviour rather than
  andris's, and an RDP client driven through `xdotool` on a shared
  runner is likely to flake.
- The PRIMARY selection, and images on the clipboard. Ryll carries
  only text on CLIPBOARD today, and chansrv takes images only as
  `image/bmp`.
- Resizing the xrdp desktop from SPICE. xorgxrdp refuses resizes from
  X clients; phase 7 drafts an upstream feature request.
- End-to-end coverage of the X11 pointer and lock-state paths (XTEST
  motion and buttons, XKB indicators) against Xvfb. Phase 4's `make
  e2e` drives only frames and keys, because ryll's control socket has
  no mouse verbs; a test could drive `X11Platform` directly.

### Bugs fixed during this work

- ryll#473: ryll counted every `VD_AGENT_REPLY` as an error, because
  it took `VD_AGENT_SUCCESS` to be 0 rather than 1. Fixed in phase 2.
- In phase 2 ryll also learned to read a pixmap with
  `PAL_FROM_CACHE` after its 8-byte palette id, as spice.proto lays
  it out.

### Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.
