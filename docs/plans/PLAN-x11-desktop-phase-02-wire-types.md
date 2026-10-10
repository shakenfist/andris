# Phase 2: Server-role wire types (ryll)

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: high, as the master plan asks. This is a refactor
of a client that people use, inside crates that kerbside consumes.
Done wrongly, it changes ryll's behaviour in ways no test notices.

## Scope

The plan lives here, beside its master plan. The code lands in ryll,
as one pull request on a ryll branch named
`x11-desktop-phase-02-wire-types`, which links back to this file.
This andris branch carries only the phase 1 close-out and this plan.

In:

- a reader and a writer in `shakenfist-spice-protocol` for every
  message andris will send or receive on the main, display, cursor
  and inputs channels, and for the vdagent messages phase 7 needs;
- the constants those types need and which ryll lacks;
- moving the vdagent constants and helpers out of the renderer;
- re-pointing ryll's renderer at the protocol-crate readers, so that
  the code ryll's client runs is the code the round-trip tests
  exercise;
- a round-trip test for every type, and fuzz targets for the new
  readers;
- verifying kerbside's tests against the new ryll revision.

Out:

- **LZ4 image payloads.** Ryll and spice-server disagree about the
  framing; see survey finding 5. Phase 3 settles it.
- Fixing ryll's agent-data reassembly (survey finding 4). It is a
  client bug with its own issue. Phase 7 depends on it.
- `VDIChunkHeader` and the `VDP_*` ports. They frame traffic
  between spice-server and a guest agent. Andris plays the agent in
  process (master plan phase 7), so it never uses them.
- Main `NAME` and `UUID`, the GL messages, `INVAL_LIST`,
  `INVAL_ALL_PIXMAPS` and cursor `TRAIL`. Andris sends none of them
  and the client does not need new parsers for them.
- Moving ryll's existing `byteorder` readers onto `BoundedReader`.
  That is ryll#136. This phase touches an existing reader only when
  it is replacing it.
- Any change to `logging::message_names`.
- A ryll release. Phase 3 releases.

## What the survey found

Checked on 2026-10-09 against ryll `4d5f9e9`, kerbside's pinned ryll
revision `fdb5ead5`, and spice-protocol and spice-server under
`/srv/src-reference/spice/`. Paths are in ryll unless they say
otherwise:

- P is `shakenfist-spice-protocol/src`.
- R is `shakenfist-spice-renderer/src/channels`.
- K is `kerbside/rust/kerbside-proxy`.

1. **The master plan's premise is wrong. There is almost nothing to
   move.** The renderer holds no wire structs. Its display, cursor,
   main and inputs code parses the wire inline at fixed byte offsets,
   for example:
   - STREAM_CREATE, R/display.rs:1494-1647;
   - the SpiceCopy body, R/display.rs:2016-2045;
   - cursor MOVE, R/cursor.rs:242-256;
   - AGENT_DATA, R/main_channel.rs:1118-1129.

   The protocol crate already holds `P/messages.rs` (1856 lines),
   which has the following types:

   | Kind | Types |
   |------|-------|
   | Readers only | `MainInit`, `ChannelsList`, `SetAck`, `Ping`, `Notify`, `SurfaceCreate`, `DrawBase`, `ImageDescriptor`, `CursorInit`, `CursorSet`, `SpiceCursorHeader` |
   | Writers only | `DisplayInit`, `KeyEvent`, `InputsKeyModifiers`, `MouseMotion`, `MousePosition`, `MouseButton`, `Ping::write_pong`, `SetAck::write_ack_sync` |
   | Both | `MessageHeader`, `make_message`, `take_message` |

   So the phase mostly *creates* protocol types and replaces inline
   parsing with them. No type has a round-trip test. The master plan
   is corrected at source. Its Situation section described these
   structures as living in the renderer, and that is fixed too.
2. **Some existing types cannot round-trip as they stand:**
   - `CursorInit` and `CursorSet` stop before the `SpiceCursor` body.
     The renderer slices that body itself (R/cursor.rs:196-238).
   - `SpiceCursorHeader::read` returns `None` for `FLAG_NONE` and
     drops the flags (P/messages.rs:923).
   - `MouseButton.button` holds a *mask*, which `write` converts to
     a button id. Unknown masks become 0 (P/messages.rs:1032-1041),
     so the conversion is lossy.
   - `Notify` maps unknown severities and visibilities to defaults
     (P/messages.rs:254-258).
   - `DrawBase` stores clip rectangles as left, top, right, bottom
     (P/messages.rs:384), which is not wire order.
   - `DrawBase` and `DisplayInit` use unsigned types where the wire
     is signed (`int32` rectangles; `int64`/`int32` cache sizes).

   Fixing these changes public types, and that is a breaking change
   (decision 8).
3. **Error policy varies by message, and the renderer encodes it:**
   - A failed `SurfaceCreate::read`, `DrawBase::read`, main `INIT`,
     `PING`, `SET_ACK` or `NOTIFY` ends the channel.
   - Short payloads for `SURFACE_DESTROY`, `MOUSE_MODE` and
     `MULTI_MEDIA_TIME` only warn or are ignored.
   - A short `AGENT_TOKEN` is treated as one token
     (R/main_channel.rs:1140-1146).

   Re-pointing the renderer at a uniform `read() -> Result` would
   silently change all of this. Decision 6 keeps each policy at its
   call site.
4. **Two ryll client bugs, both outside this phase:**
   - **VD_AGENT_REPLY success value.** `vd_agent.h:267` defines
     `VD_AGENT_SUCCESS = 1`. Ryll's doc comment and test treat 0 as
     success (R/main_channel.rs:115, :2031). As a result
     `agent_reply_error_count` counts every successful reply as an
     error.
   - **Agent-data reassembly.** spice-server forwards guest agent
     data to the client in buffers of at most 2048 bytes
     (`server/reds.cpp:752-780`, `SPICE_AGENT_MAX_DATA_SIZE`). Ryll
     treats every `AGENT_DATA` as the start of a new
     `VDAgentMessage` (R/main_channel.rs:1118). Agent messages over
     about 2 KB, such as clipboard text, are therefore truncated, and
     their continuation chunks are misread as headers.

   They are ryll#473 and ryll#474. The new `VdAgentReply` type
   documents the correct constant (decision 7).
5. **Ryll's LZ4 image decoder does not match spice-server.**
   - `spice.proto:558` makes LZ4 a `BinaryData`, which has a `u32
     data_size` prefix. Ryll reads no prefix (commit `e5eab94`,
     `docs/spice-protocol.md:251`).
   - spice-server compresses multi-line chunks as one LZ4 stream,
     carrying the dictionary from chunk to chunk
     (`server/lz4-encoder.c:57-80`). spice-common decodes them with
     `LZ4_decompress_safe_continue` (`common/canvas_base.c:579-598`).
     Ryll decodes exactly `height` independent per-row blocks
     (`shakenfist-spice-compression/src/lz4.rs:62-94`).
   - Ryll maps format bytes 0/4 to 32-bit, 6 to RGBA, 3 to 24-bit and
     2 to 16-bit (`lz4.rs:37-45`). `enums.h:217-229` has 6 as
     `16BIT`, 7 as `24BIT`, 8 as `32BIT` and 9 as `RGBA`, and ryll's
     own bitmap path accepts 8 and 9 (R/display.rs:2176).

   An encoder that follows the specification would therefore not
   decode in ryll, and a ryll-compatible encoder would not decode in
   spice-gtk. This is ryll#475. Phase 3 is corrected at source: it must settle the
   format against a capture from spice-server, and fix ryll's
   decoder, before writing an encoder.
6. **Kerbside is easy to keep working.**
   - From `messages`, kerbside imports only `MessageHeader`
     (K/src/policy.rs:14, K/src/relay.rs:18) and `make_message`
     (K/src/relay.rs:360, in a test).
   - It pins ryll `fdb5ead5` (K/Cargo.toml:25), which is 139
     commits behind `develop`.
   - One kerbside test asserts that
     `message_names::inputs_client(6) == "unknown"`
     (K/src/allowlist.rs:190). Adding names to `common_client` would
     break it.
   - Every message andris will emit is already named in
     `message_names` (P/logging.rs:322-592).
7. **The renderer already holds de facto writers in its tests:**
   - R/display.rs:4459-4531 assembles DRAW_COPY payloads;
   - R/cursor.rs:578 has `build_cursor_payload`.

   They are the starting point for the writers. Their tests switch
   over to the protocol-crate writers.
8. **Addresses inside a message are offsets from the body start.**
   - Ryll reads `src_bitmap` as an index into the message body, with
     the 6-byte mini header excluded. Zero means null
     (R/display.rs:2025, :2089-2117).
   - spice-server writes placeholder pointers, appends each pointee,
     then patches in its offset relative to the body
     (`spice-common/common/marshaller.c:420-433, 506-540`). The order
     is: fixed fields, then the source image, then the mask
     (`server/dcc-send.cpp:960-981`).
   - Ryll's `FromCache` path depends on that order
     (`docs/spice-protocol.md:258-263`).
9. **Constants that do not exist yet:**
   - clip type, bitmap format, bitmap flags, surface format, stream
     flags, mask flags and cursor type;
   - inputs `ACK_SYNC`, `ACK` and `PONG`. R/inputs.rs:506 and :526
     send the literals 1 and 3.

   The vdagent constants are private to the renderer
   (R/main_channel.rs:190-218).
10. **There are two reader styles:**
    - `P/messages.rs` uses `io::Result` with a byteorder `Cursor`;
    - `P/reader.rs:125` has `BoundedReader`, which returns
      `LinkError`.

    Ryll's `AGENTS.md` says parsers of untrusted input use
    `BoundedReader` and ship a fuzz target. None of the six existing
    fuzz targets under `shakenfist-spice-protocol/fuzz/` touches
    `messages.rs`. `tools/fuzz-targets.sh` picks up new targets
    automatically.
11. **Semver policy.** All the shared crates move together at 0.1.7.
    `docs/releasing.md:147-157` says a breaking change to a published
    crate means the next minor version, and a "Breaking changes"
    heading in the pull request. Nothing in the protocol crate is
    `#[non_exhaustive]`.

## Decisions

1. **The plan lives in andris and the code in ryll** (operator's
   choice).
   - This branch, with the phase 1 close-out and this plan, lands in
     andris first, as a docs-only pull request.
   - The ryll pull request links to this file.
   - The master plan's `Merged` cell records
     `ryll <sha> (#pr)`, and phase 3's planning commit closes this
     phase out.
2. **`messages.rs` becomes a `messages/` module directory, with the
   old paths kept.**
   - Submodules: `common` (header, framing, `PING`, `SET_ACK`,
     `NOTIFY`), `main`, `display`, `cursor`, `inputs` and `vd_agent`.
   - `messages/mod.rs` re-exports every existing public item, so
     `messages::MessageHeader`, `messages::make_message` and the
     rest keep their paths.
   - Step 1 does the split alone, with no code changes, so that
     reviewing it is a matter of checking a move.
   - About forty new types in one file would make an unreviewable
     5000-line module.
3. **One shape for every type.**
   - Readers use `BoundedReader` and return `LinkError`.
   - Writers are `fn write(&self, out: &mut Vec<u8>)` and infallible,
     because writing to a `Vec` cannot fail.
   - A `WireType` trait in `messages/mod.rs` declares both. A
     generic test helper `assert_round_trip` checks that writing then
     reading gives back an equal value, and that the reader consumes
     exactly what the writer produced.
   - A message whose layout depends on negotiated capabilities (the
     clipboard messages with and without the selection byte) does not
     implement the trait. It has `read_with` and `write_with` taking
     that context.
   - Existing `io::Result` readers are replaced only where this
     phase supersedes them. The rest are ryll#136's job.
   - The cost is two styles in one crate until #136 lands. That is
     better than putting forty new readers on the style that
     `AGENTS.md` deprecates.
4. **Types model the wire, not ryll's use of it.**
   - Every field spice.proto defines is a field, including those ryll
     ignores: the STREAM_CREATE clip, `DISCONNECTING`'s reason, the
     DRAW_COPY mask, and the `PING` padding (as a length).
   - Signed wire fields are signed.
   - Enumerated fields hold the raw integer, with typed accessors, so
     unknown values survive a round trip.
   - Readers enforce the minimum length spice.proto implies and
     ignore trailing bytes, as spice-common's demarshallers do.
5. **Every opcode name and constant comes from the reference.**
   Each new constant cites its line in `spice/enums.h`,
   `spice/protocol.h` or `spice/vd_agent.h`, and a test asserts the
   values that matter for interoperability: bitmap format 8 is
   `32BIT`, and `VD_AGENT_SUCCESS` is 1. The literal opcodes in
   R/inputs.rs become named constants with the same values.
6. **The renderer is re-pointed, but keeps its behaviour.**
   - Each inline parse site in R/main_channel.rs, R/display.rs,
     R/cursor.rs and R/inputs.rs calls the new reader.
   - Its existing error policy (end the channel, warn, ignore or
     default) moves to an explicit `match` at the call site.
   - `warn_once` keys, opcode counters, traffic recording and
     `send_with_log` stay in the renderer, and none of them moves
     into the protocol crate.
   - The only accepted behaviour change is rejecting input that
     spice.proto makes malformed, such as a 50-byte STREAM_CREATE
     with no clip type. Each one is listed in the pull request.
   - The LZ4, QUIC, LZ, GLZ and zlib-GLZ image paths are not touched.
7. **Ryll's bugs are filed, not fixed, with one exception.**
   - Agent-data reassembly (ryll#474) and the LZ4 framing
     (ryll#475) stay out of scope.
   - The `VD_AGENT_REPLY` fix is a one-line change to a diagnostic
     counter. It would be perverse for the new `VdAgentReply`
     documentation to state the right constant while the renderer
     next to it keeps the wrong one. It is fixed in its own commit
     (step 4), which closes its issue, so that it can be reverted
     separately.
8. **Breaking changes are accepted, and the next release is
   0.2.0.**
   - The types in survey finding 2 are fixed rather than duplicated
     next to correct copies.
   - Their only consumers outside ryll's workspace are kerbside's
     two imports, which keep their paths and signatures.
   - The pull request lists every break under "Breaking changes", as
     `docs/releasing.md` requires.

   This is the decision a reviewer is most likely to question,
   because purely additive types would avoid a minor-version bump.
   Additive types would leave two `CursorSet`s and two
   `MouseButton`s, one of them wrong, in a crate whose whole point is
   to be the one true definition. And phase 3 releases anyway.
9. **The DRAW_COPY writer builds the body in two passes.**
   - It writes the fixed fields with placeholder offsets for
     `src_bitmap` and the mask.
   - It appends the source image (descriptor, then payload).
   - It patches the source offset, relative to the message body.
   - It leaves the mask at 0, because andris sends no masks.
   - It emits clip rectangles inline in wire order, and a correct
     bounding box, even though ryll ignores the box's right and
     bottom (R/display.rs:2601-2604).
   - Image payloads modelled: `Bitmap` (the 18-byte header and
     pixels, with a palette offset of 0) and `BinaryData` for JPEG.
     `SpiceImage` has no LZ4 variant until phase 3 decides its
     framing.
10. **Fuzzing checks the round-trip property.** Two new targets go
    under `shakenfist-spice-protocol/fuzz/`:
    - `fuzz_server_bound`, for the readers andris runs on
      client-to-server messages;
    - `fuzz_client_bound`, for the readers ryll runs on
      server-to-client messages.

    Each dispatches on a selector byte to one reader. If the reader
    accepts the input, the target writes the value back out, reads
    it again and asserts the two values are equal. No new dependency
    is added: ryll uses no proptest, and the fuzz targets cover the
    property.
11. **`logging::message_names` does not change.** Every message
    andris emits is already named (survey finding 6). An addition
    would only put kerbside's `inputs_client(6)` test at risk.
    Step 9 verifies the file is byte-identical.
12. **Kerbside is verified, not changed.** In a scratch kerbside
    worktree:
    1. Point the ryll `rev` at the pushed phase branch head.
    2. Run `make test`.
    3. Record the result here, then throw the worktree away.

    Kerbside's pin is 139 commits behind, so if anything fails, bump
    to ryll's `develop` base first to separate old drift from this
    phase's changes. Bumping kerbside's pin for real is kerbside's
    business.

## Execution

Steps 1 to 8 are commits on the ryll branch
`x11-desktop-phase-02-wire-types`, in a worktree at
`/srv/kasm_profiles/mikal/vscode/src/shakenfist/ryll-wt-p02`, made by
the management session after reviewing each sub-agent's diff. Every
commit must leave `make lint` and `make test` passing in ryll. Cargo
runs only through the `Makefile`, never on the host.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Complete | Land this plan; create the ryll worktree; file issues. See brief 0. |
| 1 | medium | sonnet | none | Complete | Split `messages.rs` into a module; add the `WireType` trait, round-trip helper and constants. See brief 1. |
| 2 | high | opus | none | Complete | Common and main-channel types; re-point the main channel. See brief 2. |
| 3 | high | opus | none | Complete | Vdagent types; move the vdagent constants and helpers. See brief 3. |
| 4 | low | sonnet | none | Complete | Fix the `VD_AGENT_REPLY` success value. See brief 4. |
| 5 | high | opus | none | Complete | Cursor and inputs types; re-point those channels. See brief 5. |
| 6a | high | opus | none | Complete | Display streams, surfaces and simple messages. See brief 6. |
| 6b | high | opus | none | In progress | Images and DRAW_COPY. See brief 6, as amended under "Departures from the briefs". |
| 7 | medium | sonnet | none | Not started | Two fuzz targets with the round-trip property. See brief 7. |
| 8 | low | sonnet | none | Not started | Ryll documentation. See brief 8. |
| 9 | medium | management | none | Not started | Push audit, kerbside check, pull request, queue. See brief 9. |

Steps 2, 3, 5 and 6 are high effort because each one removes inline
parsing from a channel that ryll runs, and each must keep that
channel's behaviour. The difficulty lies in the call sites, not in
the types. They run one at a time, because they all touch the
`messages` module. In every brief below, ryll paths are relative to
the ryll worktree, and P and R abbreviate as in the survey.

**Brief 0: land the plan** (management session).

- Commit the phase 1 close-out, then this plan, on this andris
  branch. Open a pull request and put it through andris's merge
  queue. It is docs-only, so the smoke tier skips its code jobs.
- `git -C ../ryll fetch origin develop:develop`, then
  `git -C ../ryll worktree add -b x11-desktop-phase-02-wire-types
  ../ryll-wt-p02 develop`.
- The ryll issues from survey findings 4 and 5 were filed during
  planning: ryll#473 (`VD_AGENT_REPLY` value), ryll#474 (agent-data
  reassembly) and ryll#475 (LZ4 framing).

**Brief 1: module split, trait and constants.**

Read `AGENTS.md` ("Modifying protocol handling") and `STYLEGUIDE.md`
first.

- `git mv P/messages.rs P/messages/mod.rs`, then move items into
  `common.rs`, `main.rs`, `display.rs`, `cursor.rs` and `inputs.rs`
  by channel, as follows:
  - `MessageHeader`, `make_message`, `take_message`,
    `ReceivedMessage`, `Ping`, `SetAck` and `Notify` go to `common`;
  - the draw types (`DrawBase`, `SpiceBrush`, `SpiceQMask`,
    `SpiceFill`, `SpiceOpaque`, ...) and `ImageDescriptor` go to
    `display`.
  - `mod.rs` keeps `pub use` lines for every item, so that every
    existing `messages::X` path still resolves. Move the existing
    tests with the code they test. Change no code in this commit;
    `git diff -M` must show only moves and `use` lines.
- Create an empty `vd_agent.rs`; step 3 fills it.
- In `mod.rs`, add:

  ```rust
  /// A SPICE message body with a fixed wire layout.
  pub trait WireType: Sized {
      /// Parse a body. Trailing bytes are ignored.
      fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError>;
      /// Append the body's wire encoding to `out`.
      fn write(&self, out: &mut Vec<u8>);
      /// Parse a whole message body.
      fn decode(body: &[u8]) -> Result<Self, LinkError> {
          Self::read(&mut BoundedReader::new(body))
      }
  }
  ```

  Channel code that used `?` on an `io::Result` reader now gets a
  `LinkError`. Convert it at the call site, or with a `From` impl
  on the channel's error type, whichever that type already uses.

  Add a `#[cfg(test)] pub(crate) fn assert_round_trip<T: WireType +
  PartialEq + Debug>(value: &T)`. It writes the value, reads it back,
  asserts equality, and asserts the reader's `position()` equals the
  written length.
- In `P/constants.rs`, add modules `clip_type`, `bitmap_fmt`,
  `bitmap_flags`, `surface_fmt`, `stream_flags`, `mask_flags` and
  `cursor_type`, plus `ACK_SYNC`, `ACK` and `PONG` in
  `inputs_client`.
  - Copy every value from `/srv/src-reference/spice/spice-protocol/
    spice/enums.h` and `protocol.h`, citing the line in a comment.
  - Add a test asserting `bitmap_fmt::BIT32 == 8` and
    `bitmap_fmt::RGBA == 9`.
  - Replace the literals `1` and `3` at R/inputs.rs:506 and :526
    with the new constants.

Commit subject: `Split the protocol crate's message types by channel.`

**Brief 2: common and main-channel types.**

Add these types in `messages/common.rs` and `messages/main.rs`,
implementing `WireType`, each with an `assert_round_trip` test and a
test that decodes a hand-written byte vector taken from spice.proto's
layout:

| Area | Types |
|------|-------|
| Common | `Ping` (with a `padding_len`, plus a writer); `Pong`; `SetAck` (writer); `AckSync`; `Notify`, replacing the lossy reader, with raw `severity` and `visibility` and a NUL-terminated message whose writer emits `message_len + 1` bytes as `main-channel-client.cpp:667-669` does; `Disconnecting` |
| Main | `MainInit` (writer); `ChannelsList` (writer); `MainMouseMode` (server, two `u16`s); `MouseModeRequest` (client, `u16`); `MultiMediaTime`; `AgentTokens` (shared by `AGENT_CONNECTED_TOKENS`, `AGENT_TOKEN` and `AGENT_START`); `AgentDisconnected` (error code) |

`ATTACH_CHANNELS`, `AGENT_CONNECTED` and `ACK` have empty bodies and
need no type.

Then re-point the renderer at these readers. `PING` and `SET_ACK`
are handled separately in every channel file, by `Ping::read(payload)?`
and `SetAck::read(payload)?` in R/cursor.rs, R/inputs.rs,
R/playback.rs, R/webdav.rs, R/usbredir.rs and others. Find them all
with `grep -rn "Ping::read\|SetAck::read\|Notify::read" R/`, and
also re-point R/main_channel.rs. Keep each site's current failure
behaviour, listed in survey finding 3:

- `INIT`, `PING`, `SET_ACK` and `NOTIFY` fail the channel;
- `MOUSE_MODE` and `MULTI_MEDIA_TIME` warn and continue;
- a short `AGENT_TOKEN` still counts as one token. Keep the quirk
  and add a comment citing finding 3.

Delete each inline helper only once nothing calls it, and move its
tests to the new type: `parse_mouse_mode_payload` (:36),
`build_mouse_mode_request_payload` (:95) and
`parse_agent_connected_tokens` (:106).

Do not touch `AGENT_DATA` or anything vdagent; that is step 3.

Commit subject: `Model the common and main channel messages.`

**Brief 3: vdagent types.**

- Move the `VD_AGENT_*` constants from R/main_channel.rs:190-218
  into a `vd_agent` module in `P/constants.rs`. Check every value
  against `spice/vd_agent.h`, citing the line. That includes
  `VD_AGENT_SUCCESS = 1`, `VD_AGENT_ERROR = 2`, the message types,
  the capability bits, the clipboard types and the selections.
- In `messages/vd_agent.rs`, add:
  - `VdAgentMessageHeader` (`protocol`, `type`, `opaque`, `size`);
  - `MonitorsConfig` and `MonConfig`. `x` and `y` are `i32`, and the
    flags include `PHYSICAL_SIZE` and its trailing
    `VDAgentMonitorMM` entries.
  - `AnnounceCapabilities`, with `request` and a `Vec<u32>` of
    capability words, not just one;
  - `VdAgentReply`, with `is_success()` defined as
    `error == VD_AGENT_SUCCESS`;
  - `ClipboardGrab`, `ClipboardRequest`, `Clipboard` and
    `ClipboardRelease`, using `read_with` and `write_with` taking a
    `has_selection: bool`.
- In the renderer:
  - replace `split_clipboard_selection` (:148),
    `split_clipboard_type` (:161), `clipboard_grab_offers` (:168),
    `build_clipboard_payload` (:180), `agent_caps_has` (:132), the
    `MONITORS_CONFIG` builder (:1368-1410), the
    `ANNOUNCE_CAPABILITIES` builder (:1318-1325) and the
    agent-message header encode and decode (:1119-1124,
    :1417-1422), moving their tests;
  - the renderer's `MONITORS_CONFIG` writes `x` as `u32`. Keep the
    bytes it sends identical: compare the old and new encodings in a
    test before deleting the old builder.
- **Do not** change how `AGENT_DATA` bodies are split into agent
  messages. That is the reassembly bug, which has its own issue.
- **Do not** yet change how the renderer interprets a reply's
  `error` field. That is step 4.
- `AgentSendQueue` (R/agent_queue.rs) stays in the renderer. Its
  chunk size is protocol, but its queue cap is client policy.

Commit subject: `Model the vdagent messages in the protocol crate.`

**Brief 4: the `VD_AGENT_REPLY` success value.**

R/main_channel.rs treats `error != 0` as failure (around :1553), and
the test at :2031 asserts that `(2, 0)` is success. Use
`VdAgentReply::is_success()` instead, so that 1 is success, and fix
the test and the doc comment at :115. Check every use of
`agent_reply_error_count`, and any documentation that describes it
(`grep -rn agent_reply_error docs/ ryll/ shakenfist-spice-renderer/`).
The commit message says `Fixes #473`.

Commit subject: `Count VD_AGENT_SUCCESS replies as successes.`

**Brief 5: cursor and inputs types.**

| Area | Types |
|------|-------|
| Cursor | `SpiceCursor`: `flags`, plus an `Option` of the header (`unique_id`, `type`, `width`, `height`, hot spot) and the shape bytes when `FLAG_NONE` is clear. Replaces `SpiceCursorHeader`. |
| Cursor | `CursorInit` and `CursorSet`, completed with their `SpiceCursor`; `CursorMove`; `CursorInvalOne`. `RESET`, `HIDE` and `INVAL_ALL` are empty. |
| Inputs, server | `InputsInit`; `KeyModifiers`; `MOUSE_MOTION_ACK` is empty. |
| Inputs, client | `KeyEvent` (down/up); `KeyScancode` (a byte string); `MouseMotion`; `MousePosition`; `MouseButton` |

`MouseButton` carries the wire's button *id* and the buttons mask
as separate fields, as spice.proto does. The renderer computes the
id from its mask at the call site; `mask_to_id` (P/messages.rs:1032)
moves there.

Every client-to-server inputs type gains a reader, because andris
parses them; every cursor and server inputs type gains a writer.

Then re-point R/cursor.rs and R/inputs.rs:

- `decode_cursor_pixels` keeps matching the cursor types it supports
  today (ALPHA, COLOR24, COLOR32), now by named constants;
- unsupported types fail exactly as they do today;
- promote `build_cursor_payload` (R/cursor.rs:578) into a test of
  the protocol-crate writer.

Commit subject: `Model the cursor and inputs messages.`

**Brief 6: display types and the DRAW_COPY builder.**

Add these display types:

- `SurfaceCreate` (writer); `SurfaceDestroy`;
- `MonitorsConfig` and `Head` (display);
- `StreamCreate`, including `flags`, `stamp`, the source size and
  the trailing clip, which ryll skips today;
- `StreamData` and `StreamDataSized`, each with a header and data
  bytes;
- `StreamClip`, `StreamDestroy` and `StreamActivateReport`;
- client to server: `DisplayInit` (now with a reader, and the
  wire's signed types), `StreamReport`, `PreferredCompression` and
  `PreferredVideoCodecType` (a list of codec bytes);
- `Clip`: type plus rectangles in wire order, signed `int32`;
- `DrawBase`, with a writer, clip in wire order, and signed rects.

Then the image and DRAW_COPY layer, following decision 9:

- `ImageDescriptor`, with a writer;
- `BitmapPayload` (the 18-byte header, a palette offset that must be
  0 when written, and pixel bytes);
- `BinaryData` (`u32` size and bytes), for JPEG;
- `SpiceImage { descriptor, payload: ImagePayload }`, where
  `ImagePayload` covers `Bitmap`, `Jpeg(BinaryData)` and
  `FromCache`;
- `SpiceCopy` (the src offset, `src_area`, `rop`, `scale_mode` and
  the mask's flags, position and offset);
- `DrawCopyBuilder`, which turns a `DrawBase`, a `SpiceCopy` and a
  `SpiceImage` into a body with patched offsets. Also a
  `DrawCopy::read` that resolves the offsets through
  `BoundedReader::sub_reader`, and rejects offsets that point into
  the fixed part or past the end.

Use the test helpers at R/display.rs:4459-4531 as the reference for
the bytes a builder must produce. Then switch those tests to the
builder and delete the helpers. The round-trip test for DRAW_COPY
writes with the builder and reads with the code the renderer uses.

Re-point R/display.rs:

- `SURFACE_CREATE`/`DESTROY`, `MONITORS_CONFIG`, every `STREAM_*`
  message, `DRAW_COPY` (base, body and image descriptor), the bitmap
  and JPEG payload headers, `DisplayInit`, `StreamReport` and the
  preference messages.
- Keep the `display:decode_failure:*` `warn_once` keys and the
  opcode counters where they are.
- Leave the LZ4, QUIC, LZ, GLZ and zlib-GLZ payload paths, and the
  Blend, Opaque and other draw types, exactly as they are, apart
  from the `DrawBase` and `SpiceCopy` they share. The SpiceCopy body
  is also used by Blend (R/display.rs:498).
- Where a stricter reader now rejects something the inline code
  accepted (a STREAM_CREATE without its clip; a bitmap whose palette
  flag says a `u64` follows), record it in a list for the pull
  request.

This is the largest step. If the diff passes about 2500 lines, stop
after the stream types and propose splitting the step into 6a
(streams and simple messages) and 6b (images and DRAW_COPY).

Commit subject: `Model the display channel messages.`

**Brief 7: fuzz targets.**

Under `shakenfist-spice-protocol/fuzz/`, add `fuzz_server_bound`
and `fuzz_client_bound` as `[[bin]]` targets, following
`fuzz_link_mess_parse`.

- The first byte of input selects a reader, and the rest is the
  body.
- `fuzz_server_bound` covers every client-to-server type.
  `fuzz_client_bound` covers every server-to-client type, plus
  `DrawCopy::read`.
- On a successful read, each target writes the value out, reads it
  again, and asserts that the two values are equal.
- `read_with` types run once with each value of `has_selection`.

Run `make fuzz-smoke-fuzz_server_bound` and
`make fuzz-smoke-fuzz_client_bound` (Makefile:345), and report what
they found.

Commit subject: `Fuzz the message readers for round-trip stability.`

**Brief 8: ryll documentation.**

- `AGENTS.md`: change "Message definitions in `messages.rs`" to the
  `messages/` module, name the `WireType` convention, and say that a
  new type ships with a round-trip test.
- `docs/spice-protocol.md`:
  - add a short section on the server role: which types have
    writers, the offset scheme for DRAW_COPY, and the fact that the
    protocol crate serves both roles;
  - fix the statements survey finding 5 shows are wrong, or mark
    them as under investigation in the issue from step 0. Keep the
    LZ4 description accurate about what ryll does today.
- `ARCHITECTURE.md:29` already says the protocol crate covers both
  roles. Check that it still reads correctly and change it only if
  it does not.

Commit subject: `Document the protocol crate's server role.`

**Brief 9: audit, kerbside, pull request** (management session).

1. Run ryll's `PUSH-AUDIT.md` over the branch against `develop`.
2. Run these checks, each of which must hold:

   ```
   git diff --exit-code origin/develop -- \
       shakenfist-spice-protocol/src/logging.rs
   make lint test
   ```

3. Push the branch, and run decision 12's kerbside check against the
   pushed head.
4. Ask the operator before opening the ryll pull request. Its body
   links this plan, and has a "Breaking changes" heading (decision
   8) and a "Behaviour changes" list (decision 6 and step 4).
5. Watch ryll's smoke tier and automated reviewer, then the merge
   queue.
6. Record the ryll merge commit and the kerbside result in this
   file. The master plan's `Merged` cell is filled in by phase 3's
   planning commit.

## Departures from the briefs

- **Step 6 split into 6a and 6b** at brief 6's size check: 6a (streams,
  surfaces, simple messages) alone was about 1900 diff lines.
- **DRAW_COPY is stored demarshalled** (6b). Decision 9 described a
  type that keeps raw offsets. A value read from input laid out
  differently from the builder would then not survive brief 7's
  read, write, read property. `DrawCopy` instead holds its images,
  and the builder and `DrawCopy::write` share one layout function.
  Image types not modelled are carried as `ImagePayload::Other`,
  bounded by the next pointee so they never swallow the mask. An
  offset into the fixed part gets its own `LinkError` variant.
- **Cursor `INIT` and `SET` are read as a head and then a cursor**
  (step 5), so that a malformed cursor still leaves the position
  applied, as before. The cursor's shape bytes sit outside the
  header's `Option`, because spice.proto puts them outside the flags
  switch.
- **`ClipboardGrab` does not model the grab serial** (step 3). The
  brief's only context was `has_selection`. Phase 7 must not
  announce `VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL` until the type is
  extended.
- **Fatal parse errors name their message** (step 2). The new
  readers' `LinkError` does not say which message failed, so each
  channel-ending decode adds `malformed <MSG>` as context.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| Re-pointing a channel changes ryll's behaviour where no test looks | Decision 6 keeps each error policy at its call site. Reviewing steps 2, 3, 5 and 6, the management session checks every removed inline parse against its replacement's failure path, and the pull request lists every intended change. Ryll's existing renderer tests must pass unmodified, apart from tests that move to the protocol crate. |
| A writer and its reader share a misunderstanding, so the round trip passes but the wire is wrong | Every type also has a test decoding hand-written bytes laid out from spice.proto (brief 2), and the interoperability-critical constants are asserted against the reference (decision 5). |
| The DRAW_COPY builder's offsets are wrong in a way ryll tolerates but spice-gtk does not | The builder is checked byte for byte against the renderer's existing helpers (brief 6), which match spice-server's layout (survey finding 8). Phase 4's kerbside smoke test is the next check. |
| The breaking changes surprise a consumer | Kerbside's two imports keep their paths (decision 2), and the kerbside check proves it. The pull request lists the breaks, and the release in phase 3 bumps the minor version. |
| Step 6 is too large to review | Brief 6 sets a size at which to stop and split. |
| Kerbside's pin is so far behind that its tests fail for unrelated reasons | Decision 12 separates old drift from this phase's changes by first bumping to the `develop` base. |

## Definition of done

- **No new message names.**
  `git -C ../ryll diff --exit-code <base> <merge> --
  shakenfist-spice-protocol/src/logging.rs` succeeds.
- **Kerbside's paths survive.**
  `grep -n "pub use\|pub fn make_message\|pub struct MessageHeader"
  shakenfist-spice-protocol/src/messages/mod.rs
  shakenfist-spice-protocol/src/messages/common.rs` shows
  `MessageHeader` and `make_message` reachable at `messages::`, and
  the kerbside check recorded below passed.
- **Every type round-trips.** Every `impl WireType for` in
  `shakenfist-spice-protocol/src/messages/` has a matching
  `assert_round_trip` call in that module's tests. The management
  session checks this with:

  ```
  for t in $(grep -rho 'impl WireType for [A-Za-z]*' \
      shakenfist-spice-protocol/src/messages | awk '{print $4}'); do
    grep -rq "assert_round_trip(&$t\|assert_round_trip::<$t" \
        shakenfist-spice-protocol/src/messages || echo "missing: $t"
  done
  ```

  This prints nothing.
- **No inline parsing remains for modelled messages.**
  `grep -n "from_le_bytes" R/main_channel.rs R/cursor.rs
  R/inputs.rs` finds nothing outside tests. The same grep on
  R/display.rs finds only the untouched image decoder paths.
- **Fuzz targets exist and smoke-run.**
  `tools/fuzz-targets.sh` lists `fuzz_server_bound` and
  `fuzz_client_bound`, and both `make fuzz-smoke-*` targets pass.
- **The REPLY fix landed with its issue.** ryll#473 is closed by
  the merge.
- **The follow-up issues are still tracked.** ryll#474 and ryll#475
  are open, or closed by later work, and the master plan's phase 7
  and phase 3 sections link them.
- **Ryll's checks pass.** The ryll pull request passed its smoke
  tier and merge queue.

## Result

Not yet run.

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan. Gate: decision 8 (breaking
changes to published types, so the next ryll release is 0.2.0)
commits ryll's next release to a minor bump. Confirm it with the
operator before step 2 begins, not only at the start.
