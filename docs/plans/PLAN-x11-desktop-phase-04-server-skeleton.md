# Phase 4: Server skeleton

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: high, as the master plan asks. This is the first
code that speaks SPICE as a server. It turns on the protocol's start
sequences, on what ryll's client actually requires (which is not
quite what spice-server sends), and on how an X server numbers its
keys.

## Scope

In scope, as one andris pull request built up in steps:

- a `lib` and `bin` split of the `andris` crate, with a module per
  concern;
- the platform seams as traits:
  - frame capture;
  - input injection;
  - cursor shapes;
  - display geometry;
- the seams' implementations:
  - a synthetic one, for tests and for load testing;
  - an X11 one, using `x11rb`;
- a TCP listener, with an optional rustls TLS listener;
- the link handshake and ticket authentication;
- one session at a time, which a new client replaces;
- the main channel:
  - `INIT`;
  - a `PING` straight after it;
  - the channel list;
  - mouse modes;
- a display channel with one primary surface the size of the root
  window. It sends LZ4 or uncompressed `DRAW_COPY` updates on a
  timer, in horizontal bands, sending only the bands that changed;
- an inputs channel that maps PC scancodes to X keycodes by XKB key
  name, and injects keys and pointer events through XTest;
- a cursor channel driven by XFixes cursor notifications;
- tests:
  - in-process tests against the synthetic seams, using ryll's
    client-side link drivers and decoders;
  - an end-to-end test that runs andris against Xvfb and drives it
    with ryll's headless mode. It runs from `make e2e` and in the
    merge-queue tier of CI;
- a manual smoke test through kerbside's static source, recorded in
  this file;
- documentation for running andris.

Out of scope:

- **XDamage, MIT-SHM, JPEG, flow-control tuning and metrics.** These
  are phase 5. This phase's changed-band check (decision 11) is a
  comparison of whole bands, not damage tracking.
- **The vdagent: resize, clipboard and `AGENT_CONNECTED`.** These are
  phase 7. `INIT` says no agent is connected.
- **Streams and audio.** These are phases 8 and 9.
- **Any change to ryll.** Gaps in ryll's crates get a local
  workaround here and a ryll issue (decision 2).
- **Automating the kerbside smoke test in CI** (decision 15). The
  master plan now gives that to phase 10.
- **Sharing `:10` with RDP.** This is phase 6. This phase runs
  against Xvfb, and against `:10` only by hand.

## What the survey found

The survey read andris's tree, ryll's shared crates at ryll's
`develop` (`502199e`, which contains phase 3's merge `cd2fe41`),
kerbside's `develop` (`aebbe92`), spice-server and spice-gtk in
`/srv/src-reference/spice/`, and the operator's X session `:10`.

Abbreviations:

- **A** is `andris/src`.
- **P** is `shakenfist-spice-protocol/src` in ryll.
- **C** is `shakenfist-spice-compression/src` in ryll.
- **R** is `shakenfist-spice-renderer/src` in ryll.
- **K** is `rust/kerbside-proxy/src` in kerbside.
- **S** is `server/` in spice-server.

Ryll line numbers are at `502199e`. Read them with
`git -C ../ryll show <rev>:<path>`.

1. **Andris is a stub.**
   - A/main.rs prints a banner, and `andris/tests/cli.rs` checks it.
   - There are no dependencies and no `lib.rs`.
   - The workspace sets edition 2024, `rust-version` 1.88, and
     `unwrap_used = "warn"` (an error under `-D warnings`).
   - `clippy.toml` allows unwrap in tests.
2. **The devcontainer is bare.**
   - `.devcontainer/Dockerfile` installs only `build-essential` and
     `pkg-config`. Its comment says system libraries "arrive with the
     code that needs them".
   - Through ryll's protocol crate, andris will build `tokio-rustls`
     and with it `aws-lc-sys`. Ryll's Dockerfile installs `cmake` for
     exactly that.
   - openh264 (finding 9) builds without nasm, as ryll's image shows.
   - There is no Xvfb.
3. **`deny.toml`** already allows the ryll git source and requires
   `rev =` pins. It has no `IJG` exception, which `jpeg-encoder`
   needs (master plan, phase 4).
4. **CI has no functional lane.**
   - Phase 1's decision 5 named the merge-queue job `merge-build` as
     the home for an Xvfb end-to-end lane.
   - The ruleset requires `Can see status`, `Can enqueue` and `Can
     merge`, so a new merge-tier job must be among `can_merge`'s
     `needs`.
   - `make lock` is the only way to change `Cargo.lock`. Cargo
     otherwise runs offline, against a bind-mounted cache.
5. **The server-role link drivers exist and are async.** They are
   generic over `AsyncRead + AsyncWrite + Unpin + Send`:

   | Function | Location |
   |----------|----------|
   | `read_link_mess` | P/link.rs:894 |
   | `send_link_reply` | :933 |
   | `send_need_secured` | :952 |
   | `generate_ticket_keypair` | :593, sync |
   | `read_auth_ticket` | :982 |
   | `send_auth_result` | :1007 |

   - `read_auth_ticket` always reads an `AUTH_SELECTION` mechanism
     first. A server must therefore refuse clients that do not
     advertise it.
   - There is no mini-header negotiation helper.
   - `read_link_mess` caps the size but not the time.
   - Use a fresh key per connection (RUSTSEC-2023-0071).
   - Kerbside's K/session.rs:155-387 is a working model of the whole
     server-side sequence.
   - P/link.rs:1513 `end_to_end_handshake_succeeds` runs the client
     drivers `perform_link` (:718) and `perform_auth` (:823) against
     the server drivers over `tokio::io::duplex`.
6. **Framing.**
   - `MessageHeader` (P/messages/common.rs:16) is the 6-byte mini
     header.
   - `make_message` (:336) frames a body.
   - `take_message(&mut Vec<u8>, max_body)` (:379) splits one
     message off a receive buffer.
   - There is no async read-one-message helper. Ryll's own channels
     hand-roll the loop (R/channels/main_channel.rs:731).
   - Bodies use the `WireType` trait (P/messages/mod.rs:30).
7. **Every message this phase sends or reads has a type** with a
   reader and a writer, except empty bodies such as `MARK`,
   `ATTACH_CHANNELS`, `ACK` and `MOUSE_MOTION_ACK`:

   | Message | Type | Location |
   |---------|------|----------|
   | `INIT` (main) | `MainInit` | P/messages/main.rs:17 |
   | `CHANNELS_LIST` | `ChannelsList` | :74 |
   | `MOUSE_MODE` | `MainMouseMode` | :118 |
   | `MOUSE_MODE_REQUEST` | `MouseModeRequest` | :148 |
   | `SET_ACK` | `SetAck` | common.rs:139 |
   | `PING` | `Ping` | :59 |
   | `PONG` | `Pong` | :112 |
   | `ACK_SYNC` | `AckSync` | :174 |
   | `SURFACE_CREATE` | `SurfaceCreate` | display.rs:1152 |
   | `SURFACE_DESTROY` | `SurfaceDestroy` | :1193 |
   | `MONITORS_CONFIG` | `DisplayMonitorsConfig` | :1262 |
   | `INIT` (display client) | `DisplayInit` | :1570 |
   | `PREFERRED_COMPRESSION` | `PreferredCompression` | :1652 |
   | `DRAW_COPY` | `DrawCopyBuilder` | :1049 |
   | `INIT` (cursor) | `CursorInit` | cursor.rs:215 |
   | `SET` (cursor) | `CursorSet` | :267 |
   | `INIT` (inputs) | `InputsInit` | inputs.rs:13 |
   | `KEY_MODIFIERS` | `KeyModifiers` | :43 |
   | `KEY_DOWN`, `KEY_UP` | `KeyEvent` | :70 |
   | `KEY_SCANCODE` | `KeyScancode` | :95 |
   | `MOUSE_MOTION` | `MouseMotion` | :114 |
   | `MOUSE_POSITION` | `MousePosition` | :145 |
   | `MOUSE_PRESS`, `MOUSE_RELEASE` | `MouseButton` | :185 |

   - `DrawCopyBuilder` (P/messages/display.rs:1049) builds a complete
     `DRAW_COPY` body from a `DrawBase`, a `SpiceImage` and a source
     `Rect`.
   - `NAME`, `UUID` and `INVAL_ALL_PIXMAPS` have no types; this phase
     does not need them.
8. **Capability constants.**
   - Ryll names the common, main and display capabilities
     (P/constants.rs:105-216), but not the inputs or cursor ones.
     `KEY_SCANCODE` is bit 0 of the inputs channel; it is filed as
     **ryll#503**.
   - `logging::message_names` (P/logging.rs:322) has a server-to-
     client table for each channel: main :361, display :415, inputs
     :472, cursor :500. Each falls back to the common table at :348.
9. **Encoders.**
   - `Bgrx::new(data, width, height, stride)` (C/encode.rs:34).
   - `encode_spice_lz4(&Bgrx) -> Vec<u8>` (:109) returns the
     `BinaryData` body, and does not decide whether compression was
     worth it.
   - `encode_spice_jpeg` (:171).
   - `MAX_IMAGE_DIMENSION` is 16384 (C/limits.rs:26).
   - openh264 is an unconditional dependency, so
     `default-features = false, features = ["encode"]` still builds
     it (**ryll#499**).
10. **What spice-server sends** (behaviour only):
    - **Main.** On the main channel it sends `INIT` with no
      `SET_ACK`, then three network-test `PING`s on TCP
      (S/reds.cpp:1791-1868; S/main-channel-client.cpp:154-171).
      `CHANNELS_LIST` answers `ATTACH_CHANNELS`.
    - **Display.** The display channel sends `SET_ACK` (window 20),
      then waits for the client's `INIT`. It then sends:
      - `INVAL_ALL_PALETTES`;
      - `SURFACE_CREATE` for surface 0 with the `PRIMARY` flag;
      - a full-surface `DRAW_COPY` with `OP_PUT`, no clip and no
        mask;
      - `MONITORS_CONFIG`, only if the client has that capability;
      - `MARK` (S/dcc.cpp:395-423).

      It stops sending while more than twice the window is unacked
      (S/red-channel-client.cpp:636-642).
    - **Inputs.** The inputs channel sends `INPUTS_INIT` with the
      lock state, and `MOUSE_MOTION_ACK` every 4 motion or position
      messages (S/inputs-channel.cpp). Key codes are set-1 bytes,
      least significant byte first, ending at the first zero byte
      (S/inputs-channel.cpp:260-275). On a client's `KEY_MODIFIERS`,
      it injects lock-key presses to match (:376-407).
    - **Cursor.** The cursor channel sends `SET_ACK` and then
      `CURSOR_INIT`. spice-gtk ignores `CURSOR_SET` until `INIT` has
      arrived.
    - **Auth.** An empty password is refused.
    - **One client at a time.** A new main link drops every existing
      client without a message (S/reds.cpp:1807-1809).
11. **What ryll's client needs** (R and P, behaviour):
    - **Link.** The server's link reply must offer `AUTH_SELECTION`,
      `AUTH_SPICE` and `MINI_HEADER` (P/link.rs:648-652).
    - **ATTACH_CHANNELS.** Ryll sends it only after a main-channel
      `PING` with `id > 0` arrives after `INIT`
      (R/channels/main_channel.rs:967-971), and it waits for the list
      with no timeout. spice-gtk sends it straight from its `INIT`
      handler. A server that does not ping leaves ryll hanging. This
      is filed as **ryll#502**, and decision 10 works around it.
    - **SET_ACK.** Ryll does not need `SET_ACK`. When it gets one, it
      replies `ACK_SYNC` on every channel, and then `ACK` every
      window messages, on display and cursor only.
    - **Mouse mode.** If `CLIENT` mouse mode is supported but not
      current, ryll sends `MOUSE_MODE_REQUEST(CLIENT)`.
    - **Display start.** On display start, ryll sends `INIT`,
      `PREFERRED_COMPRESSION` (`auto-glz` by default) and
      `PREFERRED_VIDEO_CODEC_TYPE`.
    - **Capabilities.** Ryll advertises display capability 5 (LZ4),
      and inputs bit 0 (`KEY_SCANCODE`), though it never sends
      `KEY_SCANCODE`.
    - **Keys.** Keys arrive as `KEY_DOWN`/`KEY_UP` with
      `(sc << 8) | 0xE0` for extended keys. Ryll sends
      `KEY_MODIFIERS(NUM_LOCK)` when the inputs channel starts, and
      again as an idle keepalive.
    - **Headless mode** (`ryll --headless`):
      - has no password option for `--direct`, so a password needs a
        `.vv` file and `--file`;
      - has no exit-after-N option;
      - takes a TLS certificate authority only from a `.vv` `ca=`.
    - **The control socket** (`docs/control-socket-protocol.md`)
      offers:
      - `status`, which lists the surfaces;
      - `screenshot`, as PNG or RGBA;
      - `send_key`, which takes a logical set-1 code, with `0xE04B`
        for Left;
      - a `surface_drawn` event.

      It has no mouse verbs. `tools/proxmox-smoke.py` is the
      precedent for asserting on it.
    - **Releases.** The latest release, v0.1.7, is 730 commits behind
      `develop`. It has the old LZ4 decoder, and control protocol 1.1,
      which sends extended keys wrongly. The end-to-end test must
      build ryll at andris's pinned revision.
      `cargo build --no-default-features -p ryll` is the slim
      headless build.
12. **Kerbside.**
    - **Link to the backend.** The proxy dials the backend's
      insecure port first. It retries over TLS only on `NeedSecured`
      with a `secure_port` set (K/backend.rs:100-121). It requires
      the same three common capabilities, and only warns about
      missing channel capabilities (K/backend.rs:279-290).
    - **Capabilities it offers clients:**
      - main `0xf`;
      - display `MONITORS_CONFIG`, `STREAM_REPORT`,
        `PREF_COMPRESSION` and `PREF_VIDEO_CODEC_TYPE`;
      - inputs `KEY_SCANCODE`;
      - cursor none (K/caps.rs:37-111).
    - **Allowlist.** It terminates a session on any opcode that
      ryll's `message_names` does not name, using ryll at
      `fdb5ead` (K/allowlist.rs:83-96). The message size cap is
      16 MiB.
    - **Static source.** A real static-source session needs
      MariaDB, kerbside's API, its daemon and the proxy.
      `tools/direct-qemu/lane-up.sh` and `start-kerbside.sh` are the
      recipe.
    - **Zero-verdict check.** `verify-rust-proxy.sh
      assert-firewall` checks zero firewall verdicts against a mock,
      with no database.
    - **Docs.** No kerbside page covers a custom SPICE server.
13. **The operator's session `:10` does not use evdev keycodes.**
    - `_XKB_RULES_NAMES` is `base`/`pc104`/`us`, and its keycodes
      are `xfree86`: `<AC01>` = 38, but `<LEFT>` = 100, where evdev
      has 113.
    - Xvfb uses evdev. A fixed scancode-to-keycode table would
      therefore pass in CI and send the wrong keys on `:10`.
    - The XKB key *names* are the same in both sets. That is what
      decision 4 relies on.
    - `:10` is 2212x1308, depth 24, 32 bits per pixel, LSBFirst, so
      its memory order is BGRX.
14. **Master plan claims.** The phase 4 section was accurate except
    on three points:
    - CI cannot cheaply front andris with kerbside's static source
      (finding 12).
    - "Drives it with ryll's headless mode" needs ryll built at
      andris's revision (finding 11).
    - The keycode question (finding 13) was not foreseen.

    This planning commit corrects the phase 4 and phase 10
    sections, and the index row.

## Decisions

1. **One pull request.**
   - This phase lands wholly in andris. The phase 3 close-out and
     this plan are the branch's first two commits, and the code
     follows on the same branch, as phase 1 did.
   - Phases 2 and 3 landed their plans separately only because their
     code went to ryll.
2. **Pin ryll by one git revision, and change nothing in ryll.**
   - Both crates come from the same `rev`: ryll's `develop` at the
     time step 1 runs, which must contain `cd2fe41`.
   - The compression crate is taken with
     `default-features = false, features = ["encode"]`.
   - Where ryll's crates fall short, andris works around the gap and
     the gap is filed:
     - ryll#499, openh264 is built anyway;
     - ryll#502, andris pings after `INIT`;
     - ryll#503, andris names `KEY_SCANCODE` itself, citing
       `enums.h`.
   - A ryll pull request in this phase would mean a second merge,
     a re-pin and a wait, for gaps that each cost a line here.
3. **One package, a library and a binary.**
   - `andris/src/lib.rs` holds the modules and `main.rs` is thin, so
     `andris/tests/` can drive the server in-process.
   - `server/` holds:
     - the listener;
     - link and auth;
     - the session;
     - one module per channel;
     - frame encoding.
   - `platform/` holds the traits, `synthetic.rs` and `x11/`.
   - This keeps open question 5's default: one crate, a module per
     backend.
4. **The platform seams.** Step 1 proposes exact signatures for
   review. In outline:
   - **`DisplayGeometry`** reports the size of the desktop. Resizing
     is added in phase 7.
   - **`FrameSource`** captures the whole desktop as BGRX, with its
     stride.
   - **`InputSink`** takes:
     - keys as **Linux evdev key codes** with pressed or released;
     - absolute and relative pointer motion;
     - buttons;
     - the lock state, which it can read and set.

     Evdev codes are the neutral key identity because libei uses
     them too, so a Wayland backend fits without a new seam. The
     inputs channel maps set-1 scancodes to evdev codes. The X11
     backend maps evdev codes to XKB key names, using a table
     generated from xkeyboard-config's `keycodes/evdev`, then to
     keycodes using the attached server's own XKB names
     (`GetNames`). That is what makes `:10`'s xfree86 keycodes work
     (finding 13).
   - **`CursorSource`** yields cursor shapes as ARGB with a hot spot,
     as they change.

   The synthetic implementation draws a deterministic test pattern
   that moves on request, records the input it is given, and yields
   one cursor shape.
5. **The X11 backend runs on threads, not tasks.**
   - x11rb's `RustConnection` is synchronous. The backend opens three
     connections, each on its own thread:
     - capture, which serves capture requests;
     - input injection;
     - cursor events, which blocks on XFixes events.
   - A thread talks to the tokio side through channels.
   - Separate connections keep a key press from queueing behind a
     12 MB `GetImage`.
   - At startup the backend refuses an X server whose root visual is
     not 24 or 32 bit depth with 32 bits per pixel, LSBFirst. It also
     refuses one that lacks XTest, XFixes or XKB, and says which.
6. **One session at a time.**
   - A new main-channel link replaces the current session. Its
     channels are closed without a message, as spice-server does.
   - The new session's id is a random non-zero `u32`.
   - A secondary channel must carry the current session's id;
     otherwise it gets `BadConnectionId`.
   - A channel id other than 0 gets `ChannelUnavailable`.
   - A channel type andris does not serve also gets
     `ChannelUnavailable`.
7. **The link and auth policy.**
   - **The link message** must arrive within 10 seconds.
   - **Client capabilities.** A client that does not advertise
     `AUTH_SELECTION` and `MINI_HEADER` gets `VersionMismatch`, as
     kerbside does (K/session.rs:201-216).
   - **Common capabilities in the reply:** `AUTH_SELECTION`,
     `AUTH_SPICE` and `MINI_HEADER`.
   - **Channel capabilities in the reply** are kerbside's set
     (finding 12), so that kerbside never warns:
     - main `0xf`;
     - display `MONITORS_CONFIG | STREAM_REPORT | PREF_COMPRESSION |
       PREF_VIDEO_CODEC_TYPE`;
     - inputs `KEY_SCANCODE`;
     - cursor none.

     The main set includes the two migration capabilities, which
     andris never acts on. The master plan asks for kerbside's set,
     and advertising a capability you never exercise is harmless.
   - **Keys.** A fresh RSA key for each connection.
   - **The ticket** is read from a file named by `--ticket-file`.
     Andris refuses to start if the file is readable by group or
     other, is empty, or holds more than 60 bytes after one trailing
     newline is stripped. There is no option to disable ticketing.
8. **TLS and exposure** (open question 3).
   - The plain listener defaults to `127.0.0.1:5900`.
   - `--tls-listen` with `--tls-cert` and `--tls-key` (PEM) adds a
     TLS listener. When it is set, the plain listener answers every
     link with `NeedSecured`, which is how kerbside finds the TLS
     port.
   - Andris refuses to start with a non-loopback plain listener and
     no TLS. With TLS, the plain listener only redirects, so it may
     bind anywhere.
9. **The opcode invariant holds by construction.**
   - Each channel's sender takes its opcode from a closed enum of
     that channel's server-to-client messages. Each variant maps to
     ryll's `main_server::*`, `display_server::*` and similar
     constants.
   - A unit test walks every variant of every enum and asserts that
     ryll's `logging::message_names` table for that channel names
     it.
   - `MSG_LIST` (opcode 8, finding 12) is not in any enum.
10. **The main channel.**
    - **`INIT`:**
      - `session_id`;
      - `display_channels_hint` 1;
      - supported mouse modes `SERVER | CLIENT`, with current mode
        `CLIENT`;
      - `agent_connected` 0;
      - `agent_tokens` 0;
      - `multi_media_time` as milliseconds since the session began;
      - `ram_hint` 0.
    - **`PING` with id 1 straight after `INIT`.** This lets ryll
      send `ATTACH_CHANNELS` (ryll#502). After that, a `PING` every
      30 seconds, with the round trip from `PONG` logged at debug
      level.
    - **`CHANNELS_LIST`** answers `ATTACH_CHANNELS` with display 0,
      inputs 0 and cursor 0.
    - **`MOUSE_MODE_REQUEST`.** A supported mode is applied, and
      either way `MOUSE_MODE` is sent with the current one.
    - **Ignored messages.** Agent messages and anything else are
      read and ignored, with a once-per-opcode debug log.
    - `NAME` and `UUID` are not sent.
11. **The display channel.**
    - **Start sequence**, as spice-server's (finding 10):
      1. `SET_ACK` with window 20.
      2. Wait for the client's `INIT`, validating it and otherwise
         ignoring it. Andris keeps no pixmap cache.
      3. `SURFACE_CREATE` for surface 0, `FMT_32_XRGB`, flag
         `PRIMARY`.
      4. A complete frame.
      5. `MONITORS_CONFIG` with one head, if the client has the
         capability.
      6. `MARK`.

      `INVAL_ALL_PALETTES` is not sent; andris sends no palettes.
    - **Frames are bands of 64 rows.** Each band is one `DRAW_COPY`:
      - surface 0;
      - the band's rectangle as the box;
      - no clip;
      - `OP_PUT`, scale mode 0, no mask;
      - an image whose descriptor's width and height equal the
        band's exactly.

      A band of 2212 pixels is about 560 KB raw. That keeps every
      message far below kerbside's 16 MiB cap at any desktop size up
      to 16384 pixels wide, and lets the ack window bound how much
      is in flight.
    - **Only changed bands are sent.** After the first complete
      frame, a band is sent only if its bytes differ from what was
      last sent. This is the decision a reviewer is most likely to
      question, as phase 5's territory. It is a byte comparison of
      the previous capture, not damage tracking. Without it, an idle
      2212x1308 desktop costs several megabytes a second through
      kerbside, which makes this phase's server unusable to
      dogfood. Phase 5 replaces it with XDamage, so nothing built on
      it outlives that phase.
    - **The encoding of a band:**
      - LZ4 if the client advertises display capability 5 and has
        not asked for compression `OFF`. Andris has no GLZ, LZ or
        QUIC, so every other preference means LZ4.
      - An uncompressed `BITMAP` (`BIT32`, `TOP_DOWN`, stride width
        times 4) if the client lacks capability 5, has asked for
        `OFF`, or the LZ4 output is not smaller than the raw pixels.
    - **Image ids** are a counter from 1. No image carries
      `CACHE_ME`.
    - **The timer** captures every `--frame-interval-ms`, default
      200. A tick is skipped, not queued, while more than twice the
      ack window is unacked.
    - **A size change.** If the desktop's size has changed since the
      last tick, andris:
      1. sends `SURFACE_DESTROY`;
      2. re-runs steps 3 to 6 of the start sequence with the new
         size.

      Xvfb never resizes, but `:10` does when RDP resizes.
    - **No JPEG yet.** Choosing JPEG needs phase 5's heuristic.
12. **The inputs channel.**
    - **`INPUTS_INIT`** carries the X server's current lock state.
    - **`KEY_DOWN` and `KEY_UP`** are split into set-1 bytes, least
      significant byte first, stopping at the first zero byte. They
      are then mapped to an evdev code:
      - codes 0x01-0x53 and 0x56-0x58 map to themselves;
      - extended codes map through a table, which step 5 lists.
    - **`KEY_SCANCODE`** bytes are decoded the same way.
    - **`KEY_MODIFIERS`.** For each lock whose state differs from the
      X server's, andris injects a press and release of that lock
      key (caps 0x3a, num 0x45, scroll 0x46), as spice-server does.
      This matters because ryll sends `NUM_LOCK` as a keepalive.
    - **`MOUSE_POSITION`** (client mode) must name display 0.
      Coordinates are clamped to the desktop.
    - **`MOUSE_MOTION`** (server mode) moves relative to the current
      position.
    - **Buttons** map to X buttons:
      - left 1, middle 2, right 3;
      - wheel up 4, wheel down 5;
      - side 8, extra 9.
    - **`MOUSE_MOTION_ACK`** is sent after every 4th motion or
      position message.
    - **Closing.** When the channel closes, andris releases every key
      and button it pressed and has not released, so that a dropped
      connection cannot leave a key held down on the desktop.
13. **The cursor channel.**
    - **Start.** `SET_ACK` with window 20, then `CURSOR_INIT`. It
      carries the current shape, or `SPICE_CURSOR_FLAGS_NONE` if
      there is none.
    - **Changes.** Each XFixes cursor change becomes a `CURSOR_SET`
      with an `ALPHA` cursor, `unique` 0 and no cache flags. The
      pixels are XFixes' ARGB, as little-endian 32-bit words. This
      is the layout ryll's cursor decoder reads
      (R/channels/cursor.rs:532-574).
    - **Size limit.** A shape larger than 256 by 256 is dropped, with
      a warning logged once.
    - **No `CURSOR_MOVE`.** In client mouse mode the client draws the
      pointer itself.
14. **Testing at two levels.**
    - **In-process tests (`make test`).** These run the server on a
      loopback port against the synthetic platform. They use:
      - ryll's client link drivers;
      - `DrawCopy::read`;
      - the compression crate's LZ4 decoder, as a dev-dependency
        with the `lz4` feature.

      They assert that the decoded pixels equal the synthetic frame.
    - **End-to-end (`make e2e`).** This runs andris against Xvfb,
      and ryll headless built at the pinned revision, through its
      control socket. It asserts:
      - that frames arrive, by screenshot;
      - that keys reach an X client, checked by **keysym** (`a`,
        `Left`), not keycode, so the assertion is right under any
        keycode set.

      It runs in a new merge-tier job, "End to end", which
      `can_merge` needs. A pull-request run would add ryll's build
      to every push. The management session runs `make e2e` before
      opening the pull request, so a merge-queue failure is a
      surprise rather than the first signal.
15. **Kerbside is checked by hand in this phase.**
    - Step 9 runs kerbside's direct-qemu recipe with andris in
      QEMU's place. It asserts:
      - the "hypervisor connection successful" audit event;
      - zero firewall verdicts;
      - a ryll screenshot through the proxy.
    - The result is recorded here.
    - Automating it in CI needs MariaDB, kerbside's API, its daemon
      and its proxy beside Xvfb and ryll. That belongs with phase
      10's soak, or in kerbside's own CI as a "custom backend" lane.
      The master plan's phase 10 section now says so.
16. **The dependencies:**
    - `tokio`;
    - `tokio-rustls`, at ryll's version so that only one `rustls`
      is built;
    - `rustls-pemfile`;
    - `clap` (derive);
    - `tracing` and `tracing-subscriber` (env-filter);
    - `anyhow`;
    - `rand`;
    - `x11rb`, with the `xtest`, `xfixes` and `xkb` features;
    - the two ryll crates.

    The dev-dependencies are:
    - `rcgen`, for TLS tests;
    - `tempfile`;
    - the compression crate with `lz4` (decision 14).

    The devcontainer gains:
    - `cmake`, for aws-lc-sys;
    - `xvfb`;
    - `xauth`;
    - `python3-xlib`;
    - `x11-xserver-utils`;
    - whatever ryll's slim headless build turns out to need.

    `deny.toml` gains the `IJG` exception for `jpeg-encoder`, with
    the reasoning ryll's carries.

## Execution

Every step is a commit on the andris branch
`x11-desktop-phase-04-server-skeleton`, in a worktree at
`/srv/kasm_profiles/mikal/vscode/src/shakenfist/andris-wt-p04`. The
management session makes each commit after reviewing the sub-agent's
diff.

- Every commit must leave `make lint` and `make test` passing.
- Cargo runs only through the `Makefile`, never on the host.
- No code is translated from spice-server, spice-gtk or x11spice.
  Their behaviour is cited from the survey.
- Every step adds tests for what it adds.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Complete | Commit the close-out and this plan; remove the phase 3 worktree. See brief 0. |
| 1 | high | opus | none | Complete | Dependencies, devcontainer, `deny.toml`, the lib/bin split, the CLI and ticket file, the platform traits and the synthetic platform. Gated. See brief 1. |
| 2 | high | opus | none | Complete | Listeners, TLS, link and auth, sessions, channel framing, opcode enums, acks; the in-process test client. Gated. See brief 2. |
| 3 | medium | sonnet | none | Complete | The main channel. See brief 3. |
| 4 | high | opus | none | Complete | The display channel and band encoding. See brief 4. |
| 5 | high | opus | none | Complete | The inputs and cursor channels. See brief 5. |
| 6 | high | opus | none | Complete | The X11 backend. See brief 6. |
| 7 | high | opus | none | Complete | `make e2e` against Xvfb and ryll headless, and its CI job. See brief 7. |
| 8 | medium | sonnet | none | Complete | Documentation. See brief 8. |
| 9 | medium | management | none | Complete | The kerbside smoke test. See brief 9. |
| 10 | medium | management | none | In progress | Push audit, pull request, queue. See brief 10. |

Steps 1 and 2 are gated (see the back brief) because their shape is
cheap to propose and expensive to change once four channels sit on
it. Steps 4, 5 and 6 are high effort because each holds a protocol
or X11 invariant that the tests can only partly see:

- the ack window;
- stuck keys;
- the keycode lookup;
- the pixel format.

In every brief, A, P, C, R, K and S abbreviate as in the survey.
Ryll paths are read at the pinned revision.

**Brief 0: land the plan** (management session).

- Commit the phase 3 close-out, then this plan, on this branch. Do
  not open a pull request yet; the branch becomes this phase's one
  pull request in step 10.
- Remove `../andris-wt-p03`, whose two local commits the close-out
  carries. First check that it has no uncommitted changes.

**Brief 1: foundations.**

Before writing code, reply with a design note of no more than a page
for the management session to approve. It covers:

- the module tree;
- the four trait signatures (decision 4);
- how the synthetic platform is configured.

Wait for approval before writing code. Then:

1. **The ryll pin.**
   - Run `git -C ../ryll fetch -q origin` and take
     `origin/develop`'s commit. Confirm it contains `cd2fe41` with
     `git merge-base --is-ancestor`.
   - Add both crates to `andris/Cargo.toml` with
     `git = "https://github.com/shakenfist/ryll"` and that `rev`.
   - Take `shakenfist-spice-compression` with
     `default-features = false, features = ["encode"]`.
   - Add a comment naming the rev's date, and saying that both
     crates must move together.
2. **The other dependencies** are those in decision 16. Run
   `make lock`, then `make build`.
   - If `make lock` churns unrelated lockfile entries (it did in
     ryll for `windows-sys`), keep only the new entries, and check
     with `make fetch` (`--locked`).
3. **The devcontainer.**
   - In `.devcontainer/Dockerfile`, add `cmake` with a comment
     naming aws-lc-sys, matching ryll's Dockerfile comment.
   - Add the Xvfb packages in step 7, not here.
4. **`deny.toml`.**
   - Add `exceptions = [{ allow = ["IJG"], crate = "jpeg-encoder" }]`
     with the reasoning from ryll's `deny.toml` (the IJG licence is
     permissive and needs only an acknowledgement).
   - Leave the bans and sources sections alone. Note: CI runs
     `cargo deny check --all-features`.
5. **The lib and bin split.**
   - `andris/src/lib.rs` declares `pub mod config; pub mod platform;
     pub mod server;`, the last as an empty shell for step 2.
   - `main.rs` keeps the banner as a `--version`-style log line, and
     calls into the library.
   - Update `andris/tests/cli.rs`, which checks the banner on stdout,
     to whatever `--version` now prints. Keep a test of it.
6. **`config.rs`** holds a clap `Args` for:
   - `--listen` (default `127.0.0.1:5900`);
   - `--tls-listen`, `--tls-cert`, `--tls-key`;
   - `--ticket-file` (required);
   - `--display` (default: `$DISPLAY`);
   - `--platform x11|synthetic` (default x11);
   - `--synthetic-size WxH` (default `1024x768`);
   - `--frame-interval-ms` (default 200, minimum 20).

   Validation, each with a unit test:
   - **TLS options:** the three are all present or all absent.
   - **Non-loopback plain listener** without TLS is refused
     (decision 8).
   - **Ticket file:** refused if its mode has any group or other
     bit, if it is empty, or if it holds more than 60 bytes after
     one trailing `\n` is stripped.
   - **Error messages** name the option and say what to do.
7. **Logging.** Use `tracing-subscriber` with `RUST_LOG`, defaulting
   to `info`.
8. **`platform/mod.rs`** holds the four traits from decision 4.
   - They are `Send + Sync`, or are owned by one task. Say which in
     the design note.
   - A frame is `{ width, height, stride, data: Vec<u8> }` in BGRX.
     Use a `Frame` type that checks `stride >= width * 4` and the
     data length at construction, as ryll's `Bgrx::new` does
     (C/encode.rs:34).
9. **`platform/synthetic.rs`.**
   - **The pattern.** A deterministic one: vertical colour bars,
     plus a 64x64 square at a position that `advance()` moves by 8
     pixels. Tests can then see which bands changed.
   - **Input.** It records every input call in a
     `Mutex<Vec<InputEvent>>` that tests can read.
   - **Lock state.** It holds settable lock state.
   - **The cursor.** It yields one 32x32 arrow-ish ARGB cursor at
     startup.
   - **Size changes.** It can be told to change size, for step 4's
     size-change test.
10. **Out of scope here:** the server, and the x11rb code. Add
    `x11rb` to `Cargo.toml` here so the lock churn happens once.

Commit subject: `Add andris's dependencies and platform seams.`

**Brief 2: links, sessions and channels.**

Before writing code, reply with a design note of no more than a page
for the management session to approve. It covers:

- the task model: which tasks exist for a session and a channel, who
  owns each socket half, and how closing a session closes its
  channels;
- the per-channel sender API;
- how the ack window is shared with step 4.

Wait for approval before writing code. Then, in `server/`:

1. **Listeners.**
   - **The plain listener**, and an optional TLS one using
     `tokio_rustls::TlsAcceptor`. The certificate chain and key come
     from PEM via `rustls-pemfile`.
   - **TLS configured.** The plain listener reads the link message
     and answers with `send_need_secured` (P/link.rs:952), as
     K/listen.rs:139-146 does.
   - **Stream type.** Wrap each accepted stream so both kinds share
     one code path. Ryll's `SpiceStream` (P/link.rs:40) has a
     `TlsServer` variant you may reuse.
2. **Link and auth**, per connection, following
   K/session.rs:155-387 for the order:
   1. `read_link_mess` (P/link.rs:894) under a 10-second
      `tokio::time::timeout`.
   2. Check the client's common capabilities for `AUTH_SELECTION`
      and `MINI_HEADER`. If either is missing, send a
      `VersionMismatch` link reply (`SpiceLinkReply::error_reply`,
      P/link.rs:416) and close.
   3. Check the channel type and id, and the connection id for
      secondary channels (decision 6). On failure, send the error
      reply and close.
   4. `generate_ticket_keypair` (P/link.rs:593), inside
      `spawn_blocking` because RSA key generation is slow.
   5. Send `send_link_reply` with the capability words from decision
      7. Define `INPUTS_CAP_KEY_SCANCODE: u32 = 1 << 0` in andris,
      citing spice-protocol's `enums.h` and ryll#503.
   6. `read_auth_ticket` (P/link.rs:982) under a timeout. Compare the
      password with the ticket in constant time (a simple xor-fold
      is fine; say so in a comment).
   7. `send_auth_result` (P/link.rs:1007) with `Ok` or
      `PermissionDenied`.
3. **Sessions** (decision 6).
   - A `SessionRegistry` holds at most one session: an id and a
     cancellation handle (`tokio_util`'s `CancellationToken`, or a
     `watch` channel).
   - A main link creates a session and cancels the previous one.
   - Each channel task ends when its session is cancelled, and drops
     its socket.
4. **Channels.**
   - **The read loop** reads into a `Vec<u8>` and takes messages with
     `take_message(&mut buf, max_body)` (P/messages/common.rs:379),
     as R/channels/main_channel.rs:731 does. The `max_body` limits
     are:
     - 4 KiB on inputs and cursor;
     - 64 KiB on main and display.
   - **Common opcodes** are handled once for every channel:
     - `PONG`;
     - `ACK_SYNC`;
     - `ACK`;
     - `DISCONNECTING`, which closes.
   - **Unknown opcodes** are ignored, with a debug log.
5. **Opcode enums** (decision 9).
   - Write one enum per channel:
     - `MainOpcode`;
     - `DisplayOpcode`;
     - `InputsOpcode`;
     - `CursorOpcode`.

     Each lists only what this phase sends. Common messages (`SET_ACK`,
     `PING`) appear in each channel's enum that sends them.
   - The sender takes `(opcode enum, body)` and frames it with
     `make_message`.
   - Add `opcodes_are_named_by_ryll`. For every variant of every
     enum, it asserts that the matching
     `shakenfist_spice_protocol::logging::message_names::*_server`
     function (P/logging.rs:361, 415, 472, 500) does not return
     `"unknown"`.
6. **Ack accounting**, for display and cursor.
   - Send `SetAck { generation: 1, window: 20 }`.
   - Count the messages sent. Subtract the window on each `ACK`,
     once the client's `ACK_SYNC` has named the current generation.
   - Expose `may_send()`, which is false while more than twice the
     window is unacked.
7. **The test client** goes in `andris/tests/support/mod.rs`.
   - It connects to a running server, and links and authenticates
     with ryll's `perform_link` and `perform_auth` (P/link.rs:718,
     823), as P/link.rs:1513 does.
   - It then reads framed messages with `take_message`.
   - Add tests for each of these:
     - a good ticket;
     - a wrong ticket, which gets `PermissionDenied`;
     - a client without `AUTH_SELECTION`, using
       `perform_link_with_caps` (P/link.rs:768), which gets
       `VersionMismatch`;
     - a secondary channel with the wrong connection id, which gets
       `BadConnectionId`;
     - a second main link, which closes the first session's
       channels;
     - the TLS listener, with an `rcgen` certificate and a
       `tokio-rustls` client;
     - the `NeedSecured` redirect.
8. **Channel handlers.** Wire main, display, inputs and cursor to
   stub handlers that steps 3 to 5 fill in. Until then, a stub
   accepts the channel and reads messages.

Commit subject: `Accept SPICE links and authenticate tickets.`

**Brief 3: the main channel.**

Implement decision 10 in `server/main_channel.rs`.

- **Messages.** Use `MainInit`, `Ping`, `ChannelsList`/`ChannelEntry`,
  `MainMouseMode` and `MouseModeRequest`
  (P/messages/main.rs:17, 65, 74, 118, 148; common.rs:59).
- **Channel numbers** come from `ChannelType` (P/constants.rs:13).
- **`multi_media_time`** is milliseconds since the session started,
  as a `u32` that wraps.
- **Pings.**
  - The first `PING` has id 1, sent straight after `INIT`. Then send
    one every 30 seconds with increasing ids.
  - The timestamp is the session's monotonic clock in nanoseconds.
  - On `PONG`, log the round trip at debug level.
- **The mouse mode** belongs to the session, because the inputs
  channel needs it to choose between position and motion handling.
  Share it with a `watch` channel.
- **Tests, with the step 2 client:**
  - `INIT` arrives first, with the decision 10 values;
  - a `PING` with id 1 follows;
  - `ATTACH_CHANNELS` yields the three-entry list;
  - `MOUSE_MODE_REQUEST(SERVER)` yields `MOUSE_MODE` with current
    `SERVER`;
  - an unknown opcode does not close the channel.

Commit subject: `Serve the main channel.`

**Brief 4: the display channel.**

Implement decision 11 in `server/display.rs`, with the encoding in
`server/frames.rs`.

1. **Start sequence** (decision 11). Read the client's `DisplayInit`
   (P/messages/display.rs:1570), and `PreferredCompression` (:1652)
   whenever it arrives.
   - **`INIT` validation.** Refuse, by closing the channel, an
     `INIT` whose pixmap cache size is negative, or whose glz window
     is negative. Andris uses neither, but they are the client's
     claims (master plan, phase 4).
2. **`SURFACE_CREATE`.** Use `SurfaceCreate` (:1152) with
   `surface_fmt::FMT_32_XRGB` and the `PRIMARY` flag. Check the flag
   constant in P/constants.rs before using it.
3. **`MONITORS_CONFIG`.** Use `DisplayMonitorsConfig`/`DisplayHead`
   (:1262, :1213) with one head: id 0, surface 0, the full size at
   (0, 0), and `max_allowed` 1. Send it only if the client advertised
   `DISPLAY_MONITORS_CONFIG`.
4. **Bands.** For each 64-row band (the last may be shorter):
   1. Compare the band's bytes with the previous capture's. Skip the
      band if it is unchanged, except in a complete frame.
   2. Build a `Bgrx` over the band (C/encode.rs:34).
   3. Encode it with `encode_spice_lz4` (:109) if decision 11 allows.
   4. Fall back to a `BitmapPayload` (P/messages/display.rs:669) with
      `bitmap_fmt::BIT32`, `bitmap_flags::TOP_DOWN` and stride
      `width * 4`, if LZ4 is not allowed or not smaller.
   5. Wrap the result in a `SpiceImage` whose `ImageDescriptor`
      (:535) has a fresh id, flags 0, and the band's width and
      height.
   6. Build the body with `DrawCopyBuilder::new(&base, &image,
      src_rect).rop_descriptor(ropd::OP_PUT).build()` (:1049-1126).
      `base` is `DrawBase` for surface 0, with the band's rectangle
      and `Clip::none()`. `src_rect` is `(0, 0, w, h)`.
5. **Timer.**
   - Use `tokio::time::interval` with `MissedTickBehavior::Skip`.
   - On each tick:
     1. If `may_send()` is false, skip the tick.
     2. Check the desktop's size. On a change, send
        `SURFACE_DESTROY` (:1193) and re-run the start sequence from
        `SURFACE_CREATE`.
     3. Capture the frame, through `spawn_blocking` or the X11
        thread's channel.
     4. Send the changed bands.
6. **`PREFERRED_VIDEO_CODEC_TYPE` and `STREAM_REPORT`.** Read and
   ignore them, after checking they parse.
7. **Tests**, against the synthetic platform:
   - the start sequence's order;
   - every band decodes, with `DrawCopy::read` and the compression
     crate's `decompress_spice_lz4`, to exactly the synthetic
     frame;
   - after `advance()`, only the bands the square crossed are sent;
   - a client without capability 5, or asking for `OFF`, gets
     `BITMAP`s;
   - no messages are sent while acks are withheld beyond twice the
     window, and sending resumes after `ACK`s;
   - a size change yields `SURFACE_DESTROY`, then a new
     `SURFACE_CREATE` at the new size, then a complete frame;
   - every descriptor's width and height equal its band's.

Commit subject: `Serve the display channel.`

**Brief 5: the inputs and cursor channels.**

Implement decisions 12 and 13 in `server/inputs.rs`,
`server/keymap.rs` and `server/cursor.rs`.

1. **Scancode decoding** (`keymap.rs`).
   - Turn a `KeyEvent` code into set-1 bytes, least significant byte
     first, stopping at the first zero byte. A byte with 0x80 set is
     a release.
   - Map to evdev codes:
     - 0x01-0x53 and 0x56-0x58 map to themselves;
     - after 0xE0, map through this table:

       | Set-1 | Evdev | Key |
       |-------|-------|-----|
       | 0x1C | 96 | KPENTER |
       | 0x1D | 97 | RIGHTCTRL |
       | 0x35 | 98 | KPSLASH |
       | 0x37 | 99 | SYSRQ |
       | 0x38 | 100 | RIGHTALT |
       | 0x47 | 102 | HOME |
       | 0x48 | 103 | UP |
       | 0x49 | 104 | PAGEUP |
       | 0x4B | 105 | LEFT |
       | 0x4D | 106 | RIGHT |
       | 0x4F | 107 | END |
       | 0x50 | 108 | DOWN |
       | 0x51 | 109 | PAGEDOWN |
       | 0x52 | 110 | INSERT |
       | 0x53 | 111 | DELETE |
       | 0x5B | 125 | LEFTMETA |
       | 0x5C | 126 | RIGHTMETA |
       | 0x5D | 127 | COMPOSE |

   - Check that every scancode ryll can send maps somewhere. Those
     are in `scancode_for_logical_key` and `char_to_scancode`
     (R/channels/inputs.rs:1212-1307, 1371+). Unmapped codes are
     logged once and dropped.
2. **The inputs handler.**
   - Send `InputsInit` (P/messages/inputs.rs:13) with the
     `InputSink`'s lock state.
   - Handle `KeyEvent`, `KeyScancode`, `KeyModifiers`,
     `MousePosition`, `MouseMotion` and `MouseButton`
     (:43-185) per decision 12.
   - Count motions and positions, and send `MOUSE_MOTION_ACK` every
     4th.
   - **Releasing held input.** Track the keys and buttons that are
     held. On channel close, release them in a `Drop` guard or a
     `finally`-style block that runs on every exit path, including
     session replacement.
3. **The cursor handler.**
   - Send `SET_ACK`, then `CursorInit` (P/messages/cursor.rs:215)
     with the current shape or `SpiceCursor::none()` (:88).
   - On a shape from `CursorSource`, send `CursorSet` (:267) with:
     - `CursorHeader` (:17): `unique` 0, `cursor_type::ALPHA`,
       width, height and hot spot;
     - the ARGB words as little-endian bytes.
   - Respect the ack window as in brief 4.
   - Drop shapes over 256x256 (decision 13).
4. **Tests**, against the synthetic platform:
   - `KEY_DOWN` 0x1E becomes evdev 30, and `0x4BE0` becomes evdev
     105, each press and release;
   - `KEY_MODIFIERS` with a lock state different from the sink's
     injects that lock key;
   - `MOUSE_POSITION` on display 1 is ignored;
   - coordinates beyond the desktop are clamped;
   - wheel buttons map to 4 and 5;
   - the fourth motion yields `MOUSE_MOTION_ACK`;
   - closing the channel with a key held releases it;
   - `CURSOR_INIT` precedes `CURSOR_SET`, and `CursorSet::read`
     round-trips the synthetic shape.

Commit subject: `Serve the inputs and cursor channels.`

**Brief 6: the X11 backend.**

Implement the four traits in `platform/x11/` with `x11rb`
(decision 5).

1. **Connecting.** Connect with `x11rb::connect(Some(display))` once
   per thread. At startup, check:
   - the root visual's depth is 24 or 32, with 32 bits per pixel;
   - the image byte order is LSBFirst;
   - XTest, XFixes and XKB are all present.

   Each refusal names what is missing.
2. **Capture.**
   - Use `GetImage` on the root window, `ZPixmap`, all planes. The
     reply's bytes are BGRX on LSBFirst at 32 bits per pixel, with
     stride width times 4.
   - Run it on the capture thread, which answers requests from a
     channel.
   - The geometry comes from the root window's `GetGeometry`, read on
     each tick so that a RandR change is seen.
3. **Input.**
   - **Keys.** Use XTest `FakeInput`.
   - **Keycodes.** At startup:
     1. Read the server's key names with XKB `GetNames` (the
        `KEY_NAMES` component).
     2. Map each evdev code to a key name with a table **generated
        from xkeyboard-config's `keycodes/evdev`**. On the host it is
        at `/usr/share/X11/xkb/keycodes/evdev`, package `xkb-data`,
        MIT licence. A name's evdev code is its keycode minus 8.
     3. Commit the generated table as `platform/x11/evdev_names.rs`,
        with a header naming its source and licence, and the script
        that made it as `scripts/gen-evdev-names.py`.
     4. Resolve aliases using the server's `KEY_ALIASES`.
   - **Lock state** is read from XKB indicator state, finding the
     `Caps Lock`, `Num Lock` and `Scroll Lock` indicators by name.
   - **Pointer motion.** Absolute motion is XTest motion to root
     coordinates, and relative motion is XTest relative motion.
   - **Buttons** use XTest button events, numbered per decision 12.
4. **The cursor.**
   - On the cursor thread, select XFixes `DisplayCursorNotify` on the
     root. On each notification, read `GetCursorImage`.
   - Its pixels are ARGB 32-bit words; pass them through unchanged.
   - Send the first image at startup.
5. **Tests.**
   - Unit-test the keycode resolution against two fixture name
     tables, built from the host's `keycodes/evdev` and
     `keycodes/xfree86`. `LEFT` must resolve to 113 and 100
     respectively; this is finding 13.
   - Everything that needs a live X server is tested by step 7.
6. **Check by hand** against Xvfb, if the step 7 tooling is not yet
   there. Do not test against `:10` from this step; the management
   session does that in step 9.

Commit subject: `Capture and drive an X11 display.`

**Brief 7: end to end against Xvfb and ryll.**

1. **The devcontainer** gains `xvfb`, `xauth`, `python3-xlib` and
   `x11-xserver-utils`.
2. **`make ryll-headless`** builds ryll at the revision `Cargo.lock`
   pins.
   - Read the revision from the `shakenfist-spice-protocol` entry's
     `source = "git+https://github.com/shakenfist/ryll?rev=...#<sha>"`
     line, so it has one source of truth.
   - Build with `cargo install --locked --git
     https://github.com/shakenfist/ryll --rev <sha> --no-default-features
     ryll --root .cargo-cache/ryll/<sha>`, under `DOCKER_RUN`, which
     has network access.
   - Skip the build if that binary already exists.
   - Add any packages ryll's slim build needs to the devcontainer,
     and say which in a comment.
3. **`make e2e`** runs `scripts/e2e.sh` under `DOCKER_RUN_OFFLINE`
   after `make build ryll-headless`. The script:
   1. Starts `Xvfb :99 -screen 0 1024x768x24`.
   2. Starts `scripts/e2e-keylogger.py`. This is python3-xlib: it
      maps a full-screen window filled with a known colour, takes
      input focus, and prints the keysym name of each `KeyPress`,
      one per line.
   3. Writes a 0600 ticket file.
   4. Starts andris on `127.0.0.1:5930` against `:99`.
   5. Writes a `.vv` file (`[virt-viewer]`, `type=spice`, `host`,
      `port`, `password`) and starts `ryll --headless --file <vv>
      --control-socket <sock>`. Keep the socket path short, under
      `/tmp`. A long path fails to bind, and only with a warning
      (ryll#488).
   6. Runs `scripts/e2e-drive.py`, which:
      - waits up to 30 seconds for `status` to show a surface;
      - takes a `screenshot` in RGBA and asserts that the centre
        pixel is the keylogger's colour;
      - sends `send_key` presses for 0x1E and 0xE04B, and asserts
        that the keylogger printed `a` and `Left` within 5 seconds.

      Follow `tools/proxmox-smoke.py` in ryll for the control-socket
      client.
   7. Stops ryll with SIGINT and expects exit 0.
   8. Fails if andris's log contains `ERROR`, or if any process died
      early.

   - Python style: single quotes, 120 columns.
   - Every wait has a timeout, and the script prints each process's
     log on failure.
4. **The CI job "End to end"** goes in `.github/workflows/ci.yml`.
   - It runs on `merge_group` and `workflow_dispatch`, like
     `merge-build`, on the same runner labels.
   - It runs `make e2e`. Its cargo-cache `key-prefix` must differ
     from the offline jobs', because `ryll-headless` writes to the
     cache with network access (`.github/actions/cargo-cache`).
   - Add it to `can_merge`'s `needs`. Run actionlint through
     pre-commit.
5. **The management session runs `make e2e` locally** before
   committing this step.

Commit subject: `Test andris end to end against Xvfb and ryll.`

**Brief 8: documentation.**

- **`docs/running.md`** is new. It covers:
  - building;
  - the options from brief 1, with their defaults;
  - the ticket file and its rules;
  - TLS, and the loopback rule;
  - running against a display with `DISPLAY` and `XAUTHORITY`;
  - connecting with ryll through a `.vv` file;
  - fronting andris with kerbside's static source. Link kerbside's
    `docs/console-sources.md` by absolute URL; the `hypervisor_ip`,
    `insecure_port`, `secure_port` and `ticket` fields map to
    andris's options.

  Describe what exists, with no plan phases (AGENTS.md:17-20).
- **`docs/development.md`** gains `make e2e` and `make
  ryll-headless`, and the "End to end" merge-tier job, beside its
  existing gate and merge-queue sections.
- **`docs/index.md`** links the new page.
- **`ARCHITECTURE.md`.** Replace the stub's path table (lines 16-29)
  with a component inventory:
  - listener and link;
  - session;
  - the four channels;
  - frames;
  - the platform traits and their two implementations.

  Add one paragraph on how a frame moves from capture to the wire.
- **`AGENTS.md`.** Add only the new conventions:
  - `make e2e` exists and needs no host packages;
  - opcodes come from the per-channel enums (decision 9);
  - keys cross the platform seam as evdev codes.
- **`README.md`.** Change it only if its install or usage line is
  now wrong.

Commit subject: `Document running andris.`

**Brief 9: the kerbside smoke test** (management session).

1. Build andris (`make release`). Run it against Xvfb, or against
   `:10` if the operator agrees, on loopback with a ticket file.
2. In a kerbside worktree, follow `tools/direct-qemu/lane-up.sh` and
   `start-kerbside.sh`, with a static `sources.yaml` console whose:
   - `hypervisor_ip` is 127.0.0.1;
   - `insecure_port` is andris's;
   - `ticket` is andris's.

   Then mint a token with `kerbside demo token`, fetch the `.vv`,
   and run ryll headless with it.
3. Record in the Result section:
   - the "hypervisor connection successful" audit event;
   - `kerbside_proxy_firewall_verdicts_total` at 0 on the proxy's
     metrics port;
   - a screenshot taken through the proxy;
   - any capability warning.
4. If an opcode is refused, stop. That breaks decision 9's invariant
   against kerbside's older ryll pin (`fdb5ead`), and needs a
   decision.

**Brief 10: audit, pull request, queue** (management session).

1. Run `PUSH-AUDIT.md` over the branch against `develop`.
2. Run `make lint test e2e`.
3. Push, then ask the operator before opening the pull request. Its
   body:
   - links this plan;
   - names the ryll revision;
   - lists the ryll issues worked around (#499, #502, #503);
   - summarises the step 9 smoke result.
4. Check the smoke jobs, `Can enqueue` and the automated reviewer.
   Then, once the operator has enqueued it, check `merge-build`, "End
   to end" and `Can merge`.
5. Record the merge commit in the Result section. Phase 5's planning
   commit fills in the master plan's `Merged` cell.

## Departures from the briefs

Each was reviewed when its step was committed, and each commit
message says why.

- **Step 1.** `platform/` carries no ryll types: the design note's
  `Frame::rows() -> Bgrx` was dropped at review, and `server/frames.rs`
  builds the `Bgrx`. Only the compression crate's size limits are
  imported, as plain constants.
- **Step 2.**
  - At most eight handshakes run at once, so an unauthenticated flood
    cannot spend unbounded CPU on key generation. This was added at
    design review.
  - A main link's session begins after the ticket checks out but before
    `Ok` is sent, so a client that hears `Ok` always finds its session.
- **Step 4.**
  - The surface's size comes from the captured frame, not a separate
    geometry read, which closes a resize race.
  - Every display message waits for ack-window room, not only bands.
  - A compression preference that arrives after `INIT` applies from the
    next frame.
- **Step 5.**
  - Pointer messages apply only in their own mouse mode, as spice-server
    does: positions in client mode, motion in server mode. Brief 5 had
    asked for positions to be honoured in server mode as well; that was
    changed to follow spice-server.
  - Pause (`E1 1D 45`) is decoded.
  - A lock key the client holds is left alone by `KEY_MODIFIERS`.
  - "No cursor" is sent as hidden, because ryll keeps the previous image
    otherwise.
- **Step 6.**
  - A startup check of the visual's colour masks was added.
  - The cursor thread is woken on shutdown with a `ClientMessage` rather
    than left blocked.
  - XKB key names above keycode 255 are skipped, since X cannot carry
    them.
- **Step 7.**
  - The end-to-end test found that ryll never opens a channel listed
    after inputs (ryll#509), so andris lists inputs last (`f4e5098`).
  - The devcontainer has `x11-utils`, not `x11-xserver-utils`.
  - `make ryll-headless` builds in its own isolated container rather
    than under `DOCKER_RUN`, because it compiles with the network up.
- **Step 9.** This used kerbside's demo compose stack, with andris in
  QEMU's place in the kerbside container's network namespace, rather
  than the direct-qemu scripts, which need a host MariaDB. Kerbside and
  kerbside-proxy 0.6.0 came from PyPI, because a worktree's `.git` file
  cannot be read inside the image build that installs from the
  checkout.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| Keys land on the wrong keys on `:10`, because its keycodes are xfree86. | Decision 4 resolves keycodes by XKB name from the attached server. Brief 6 tests both keycode sets. Brief 9 can type on `:10`. |
| Ryll's slim headless build needs packages the devcontainer lacks, or takes long enough to make the merge tier slow. | Brief 7 adds the packages and caches the binary by revision. The management session records the e2e job's time in the Result section. If it is too slow, the fallback is a released ryll once 0.2.0 ships. |
| A dropped connection leaves a key or button held on the operator's desktop. | Decision 12's release-on-close, which brief 5 tests on every exit path. |
| `GetImage` of the whole screen every 200 ms costs too much CPU on `:10`. | The interval is an option. The management session measures andris's CPU at idle against `:10` in step 9, and records it. Phase 5's XDamage and MIT-SHM are the real fix. |
| Kerbside's ryll pin, which is older, does not name an opcode andris sends. | Brief 9 stops on any firewall verdict. Every opcode in decision 9's enums existed in ryll long before `fdb5ead`. |
| `aws-lc-sys` or `openh264-sys2` fails to build in andris's image. | Brief 1 adds `cmake` as ryll's image does. Ryll's image builds openh264 without nasm. |
| The session and task model proves wrong once four channels sit on it. | The gate on briefs 1 and 2. |
| Changed-band skipping hides an encoder bug, because the tests see few frames. | Brief 4 decodes every band against the synthetic frame, including after `advance()`. |

## Definition of done

- **The checks pass.** `make lint test e2e` passes. The pull request
  passed its smoke tier, and `merge-build`, "End to end" and `Can
  merge` in the merge queue.
- **The opcode invariant has a test.** `grep -rn
  "fn opcodes_are_named_by_ryll" andris/` matches, and the test runs
  in `make test`.
- **Andris never depends on the renderer.** `grep -c 'name =
  "shakenfist-spice-renderer"' Cargo.lock` prints 0.
- **One ryll revision.** Both ryll crates in `Cargo.lock` carry the
  same `rev=`, and `make ryll-headless` reads that revision.
- **Keys work under both keycode sets.** A test resolves `LEFT` to 113
  with evdev names and to 100 with xfree86 names.
- **No key is left held.** A test closes the inputs channel with a key
  held and sees it released.
- **The ticket rules are enforced.** Tests refuse a ticket file with
  mode 0644, an empty one, and one over 60 bytes.
- **Exposure is enforced.** A test refuses `--listen 0.0.0.0:5900`
  without TLS.
- **`deny.toml`** carries the `IJG` exception for `jpeg-encoder`,
  and `cargo deny` passes in CI.
- **Kerbside.** The Result section records the step 9 smoke run
  through kerbside, with zero firewall verdicts.
- **Documentation.** `docs/running.md` exists and is linked from
  `docs/index.md`. `ARCHITECTURE.md` no longer describes andris as a
  stub. `grep -rn "phase" docs/running.md` finds nothing.
- **The ryll gaps are tracked.** ryll#499, #502 and #503 are open, or
  closed by ryll.

## Result

Andris pins ryll at `502199e`.

**The kerbside smoke test** (step 9) ran on 2026-10-11:

- **Stack.** Kerbside's demo stack (MariaDB, kerbside 0.6.0 and
  kerbside-proxy 0.6.0 from PyPI) was set up with a static console.
  Andris ran on Xvfb in the kerbside container's network namespace, on
  127.0.0.1:5910.
- **Client.** Ryll headless (the pinned build) used a `.vv` from
  `get-console.sh`. Its results:
  - It connected main, display, cursor and inputs through the proxy.
  - Its control socket showed a 1024x768 surface.
  - A PNG screenshot came back.
  - `send_key` was accepted.
- **Kerbside's side.**
  - It logged "hypervisor connection successful" for every channel of
    each of three sessions, with no capability warning.
  - The firewall was in `enforce` mode, and no verdict counter or
    termination was recorded.
  - Every relay ended only when ryll exited. Kerbside logged a missing
    TLS `close_notify` from ryll's side, which is ryll's behaviour, not
    andris's.

**Measurements:**

- **Idle CPU.** With one client connected and an idle 1024x768 Xvfb,
  andris and Xvfb together used 1.5-1.6% of one core, at the default
  200 ms interval. Scaled to `:10`'s 2212x1308, that is about 6%. That
  is above the master plan's 2% target for a static screen, which is
  phase 5's XDamage and MIT-SHM work.
- **The e2e test.** `make ryll-headless` takes about 1.5 minutes cold.
  `make e2e` takes about 5 seconds warm.

**The push audit** (step 10):

- **Wave 1** passed.
- **Style** passed.
- **Wave 2** found nothing blocking. These were fixed:
  - **Security (`5aa2109`):**
    - handshake slots per address and a 3-second first stage;
    - a delay after a wrong ticket, with the peer logged;
    - a cap on concurrent key generation;
    - a bounded X11 input queue with a reserve for releases;
    - one channel of each type per session;
    - ticket and TLS key files checked through one open handle and
      owned by andris's user;
    - a safer ticket recipe;
    - a narrower ryll build mount.
  - **Tests (`5aa2109`):** adversarial link tests, oversized messages on
    every channel, and CLI refusals through the binary.
  - **Code quality:** a failed capture skips a tick instead of ending
    the display channel; the ticket comparison; duplicated helpers; a
    narrowing cast; and the X11 module was split.
  - **Documentation:** this section, the step statuses and three small
    corrections.

**Declined in the audit:**

- **Peak memory of a complete frame.** All bands are encoded before
  any is sent, about 33 MB for a 4K desktop. Phase 5 replaces
  whole-frame capture with damage, which changes this anyway.
- **Moving the scancode decoder into ryll's protocol crate**, so that
  encode and decode can be tested together. Recorded under the master
  plan's Future work.
- **The quadratic receive-buffer drain.** It is ryll's `take_message`,
  filed as ryll#510.
- **Shortening the display and cursor module docs.** They explain
  behaviour that the code cannot show, and `docs/running.md` has the
  operator's view.

**Ryll issues this phase filed or relies on.** Each is worked around
in andris:

- ryll#499: openh264 is unconditional;
- ryll#502: ATTACH_CHANNELS waits for a ping;
- ryll#503: inputs and cursor caps and the surface PRIMARY flag are
  unnamed;
- ryll#509: channels after inputs are never opened;
- ryll#510: `take_message` is quadratic.

**Left for later phases:**

- **The `CURSOR_SET` position** is always (0, 0). XFixes reports the
  pointer's position too. Ryll's web frontend may jump the pointer on
  each shape change.
- **The keymap** is read once at startup, so a later `setxkbmap` is not
  seen.

The pull request and its merge commit are recorded below once known.

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.

Two steps are gated beyond that. The sub-agents for steps 1 and 2
each return a design note before writing code: the traits and module
tree, then the task model. The management session approves each
note, or sends it back, before the step continues. The shape of those
two steps is cheap to propose and expensive to change once four
channels are built on it.
