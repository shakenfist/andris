# Phase 5: Damage and flow control

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: high, as the master plan asks. This phase replaces
how andris decides what to send, and how it reads the screen. It
turns on what XDamage does and does not report, on the ack window
under load, and on a lossy encoding heuristic that has to be measured
rather than guessed.

## Scope

In scope, as one andris pull request built up in steps:

- **Damage-driven updates.** XDamage on the root window says where
  the screen may have changed. Andris compares only those parts
  against what it last sent, and sends only what really changed, as
  merged rectangles. A desktop that does not change costs no capture
  and sends nothing.
- **MIT-SHM capture**, with `GetImage` kept as the fallback.
- **A scrub** that slowly re-reads the whole screen, to catch changes
  that XDamage missed.
- **Flow control.** While the client is behind, damage accumulates
  and merges; it is never queued as pixels. The frame interval
  becomes a cap on the frame rate, not a polling period.
- **`MARK` after every frame**, so ryll's FPS counter measures andris.
- **JPEG** for rectangles a measured heuristic calls photographic,
  with a lossless refresh once they stop changing.
- **Per-client metrics:** frame rate, bytes out, capture and encode
  time, ack lag and ping round trip. They are logged, and readable by
  tests.
- **Tests:** in-process tests against a synthetic platform that
  reports damage, and `make e2e` assertions that an idle desktop
  sends nothing and a small change sends a little.
- **Measurements by hand**, recorded here: idle CPU, and the frame
  rate of a scrolling full-screen terminal.
- **Documentation** of the changed and new options.

Out of scope:

- **Scroll detection with `COPY_BITS`.** Ryll renders `COPY_BITS`
  (survey finding 10), and it is the obvious next step if
  scrolling falls short of 30 fps. Step 9 measures that, and the
  Result section records the outcome. The work is not grown into
  this pull request (decision 11).
- **Video streams.** These are phase 8. A rectangle that changes on
  every frame is sent as JPEG here, not as a stream.
- **Any change to ryll.** Its gaps get an issue (ryll#510 and
  ryll#512) and, where needed, a workaround here (decision 6).
- **A metrics endpoint** on the network. The metrics are logged and
  exposed in-process only (decision 10).
- **Sharing `:10` with RDP** is phase 6. This phase runs against
  `:10` only by hand, to measure, and sends it no input.
- **The X11 pointer and lock-state end-to-end coverage** that phase
  4's review asked for. It stays in the master plan's Future work.

## What the survey found

The master plan's phase 5 section was corrected for findings 5, 6,
12 and 13 as part of this plan's commit. The corrections are:
- the timer becomes a rate cap rather than going away;
- MIT-SHM is named;
- ping round trips have been measured since phase 4;
- ryll's FPS counts `MARK` messages, and headless ryll has no
  statistics.

A later step does not need to redo them.

In the references below, A is the andris tree at `1950cdb`, and R is
ryll at `502199e`, the revision andris pins. Ryll's crates have not
changed since then, apart from `client.rs`, `lib.rs` and the
renderer's session code.

1. **Capture reads the whole root window every tick.**
   - `capture` (A `andris/src/platform/x11/capture.rs:58-69`) does a
     `GetImage` of the full root at every `frame_interval`, default
     200 ms (A `andris/src/config.rs:86`).
   - `:10` is 2212x1308 at depth 24, so each capture is about
     11.6 MB through the X socket.
   - Ten `xwd -root` captures of `:10` took 0.64 s in all, about 64
     ms each, including the cost of starting `xwd`. At that rate
     `GetImage` alone cannot reach 30 full-screen frames a second.
2. **`:10` has the extensions this phase needs.** `xdpyinfo -display
   :10` lists DAMAGE, MIT-SHM, Composite, Present, RANDR, XFIXES,
   XKEYBOARD and XTEST. The e2e test runs Xvfb, andris and ryll in
   one container (A `scripts/e2e.sh:143`). They share an IPC
   namespace and a Unix socket, so MIT-SHM is usable there too.
3. **XDamage reports on `:10`, but coarsely.**
   - A spike created a `RawRectangles` damage object on the root of
     `:10` and counted events for 10 s. The session was live, with
     terminals updating.
   - It saw 37 events, 3.7 a second, totalling 24% of the screen's
     area per second.
   - The most frequent boxes were narrow strips at the top right,
     791x68 and 506x33. There was also a 1467x1083 box, and one box
     of the whole screen.
   - So damage over-reports. Sending every damaged pixel would send
     far more than changed.
   - The spike script is in this plan's appendix.
4. **x11spice does not trust XDamage alone.** As a reference for
   behaviour only:
   - alongside damage (`display.c:112-153`) it runs a scanner
     (`scan.c:41-52`);
   - the scanner re-reads the screen as a grid of 32 scanlines by 32
     tiles at between 1 and 30 scans a second, to catch changes
     damage missed.
5. **The display loop today** (A `andris/src/server/display.rs:94-112`)
   works like this:
   - It is an interval timer.
   - A tick is skipped while more than twice the ack window is
     unacknowledged.
   - Each tick captures the whole screen and sends the 64-row bands
     whose bytes differ from the last capture (A
     `andris/src/server/frames.rs:56-70`).
   - `MARK` is sent once, after the surface is created (A
     `display.rs:272`), and never again.
6. **Ryll's FPS is the rate of `DISPLAY_MARK` messages.** The window
   is the last 120 (R `ryll/src/app.rs:84`, `:2056-2063`), and the
   figure is shown as "FPS" in the status bar and in snapshots. With
   one `MARK` per surface, ryll shows andris no frame rate at all.
7. **Headless ryll has no statistics** a test can read. Filed as
   ryll#512.
   - Its periodic `Stats:` line reports zero bytes, because nothing
     emits `ChannelEvent::Statistics`.
   - It computes no FPS.
   - Its control socket has no statistics method.
   - A test can still subscribe to `surface_drawn` events. One fires
     per draw (R `ryll/src/control/server.rs:694-760`), and it counts
     draws, not frames.
8. **JPEG needs no capability.**
   - Ryll decodes `SPICE_IMAGE_TYPE_JPEG` inside `DRAW_COPY` (R
     `shakenfist-spice-renderer/src/channels/display.rs:2581-2612`),
     taking the JPEG's own dimensions.
   - `JPEG_ALPHA` is not supported (`:2662-2670`).
   - The encoder is `encode_spice_jpeg(&Bgrx, quality: u8)` (R
     `shakenfist-spice-compression/src/encode.rs:171`). It takes
     quality 1 to 100 (spice-server uses 85) and forces 4:2:0
     subsampling. It returns the `BinaryData` body for
     `ImagePayload::Jpeg`.
9. **Ryll's compression preference.**
   - Ryll sends `PREFERRED_COMPRESSION` once, after `INIT` (R
     `display.rs:1067-1087`).
   - It sends the value of `--preferred-compression`, default
     `auto-glz` = 2.
   - The values are: off 1, auto-glz 2, auto-lz 3, quic 4, glz 5,
     lz 6, lz4 7 (R `shakenfist-spice-protocol/src/constants.rs:354-363`).
10. **Ryll renders more than `DRAW_COPY`.**
    - `COPY_BITS` is decoded and rendered with memmove semantics
      (R `display.rs:1314`, `:2994`), and is in `message_names`.
    - `RECTS` clips are honoured.
    - `DRAW_FILL` works for a solid brush with `OP_PUT`.
    - These are the tools for scroll detection later (decision 11).
11. **Many small messages cost ryll more than few large ones.**
    - ryll#510 is open: `take_message` drains its buffer once per
      message, which is quadratic.
    - Each draw is also an awaited event through a bounded channel
      (R `shakenfist-spice-renderer/src/session.rs:60`).
12. **Ping round trips are already measured.** The common `PONG`
    handler logs the round trip at debug level (A
    `andris/src/server/channel.rs:354-361`). Ryll's own "latency" is
    the interval between server pings, not a round trip (R
    `channels/main_channel.rs:943-977`). Phase 5 only has to surface
    andris's figure.
13. **The master plan's success criterion** says the frame rate is
    "measured by ryll's statistics". Today that means the GUI's
    `MARK`-rate FPS (findings 6 and 7).
14. **x11rb 0.13.2 has `damage` and `shm` features.**
    - Andris enables only `xtest`, `xfixes` and `xkb` (A
      `andris/Cargo.toml:30`).
    - Andris has no `unsafe` code today.
    - Since andris#7 it depends on `libc` directly.
15. **Phase 4's idle cost.** Andris and Xvfb together used 1.5–1.6%
    of a core at 1024x768, at the 200 ms interval. Scaled to `:10`,
    that is about 6%, against the master plan's 2% target for a
    static screen.
16. **spice-gtk tolerates repeated `MARK`s.**
    - `display_handle_mark` (`spice-gtk/src/channel-display.c:1219-1231`)
      sets a flag and emits a signal.
    - It warns about a second mark only in `EXTRA_CHECKS` builds.

## Decisions

1. **Damage drives updates, and the interval becomes a cap.**
   - Nothing is captured until damage arrives.
   - `--frame-interval-ms` keeps its name, but now means the shortest
     time between two frames. Its default falls from 200 to 33, about
     30 fps, which the master plan's scrolling criterion needs. Its
     minimum stays at 20.
   - Changing what an existing option means is the call a reviewer
     might question. A new option name would leave the old one
     meaning nothing, and there are no users yet beyond the operator.
     So docs/running.md says what it means now, and the commit says
     what changed.
2. **Damage says where to look; a comparison says what to send.**
   - Damage accumulates as a region. At frame time andris captures
     the region's merged rectangles.
   - It compares each 64x64 tile in them against the shadow: the
     pixels it last sent.
   - Only changed tiles are sent. Survey finding 3 shows damage
     over-reports by orders of magnitude, so sending damage as it
     comes would undo most of the gain.
3. **MIT-SHM, through a memfd.**
   - Capture uses `ShmGetImage` into a segment that andris creates
     with `memfd_create`. The segment is passed with `ShmAttachFd`
     (MIT-SHM 1.2), so SysV IPC namespaces do not matter.
   - Andris falls back to `GetImage`, logging which it uses once at
     startup, when any of these holds:
     - the server lacks MIT-SHM 1.2;
     - the attach fails;
     - the display is reached over TCP.
   - This brings andris's first `unsafe` code, for the mapping.
     - It is confined to `andris/src/platform/x11/shm.rs`.
     - The crate root gets `#![deny(unsafe_code)]`, which that module
       alone overrides.
     - Every `unsafe` block carries a `// SAFETY:` comment.
     - The mapping is read only after the reply to a `ShmGetImage`
       has arrived, and is copied out at once.
   - `memmap2` provides the mapping, so andris does not hand-roll
     `munmap`.
4. **Damage tracking lives in the platform; the shadow lives in the
   server.**
   - The platform turns X events into an accumulated region, and
     captures rectangles on request.
   - The display channel owns the shadow and the tile comparison.
     So:
     - the comparison is tested without X;
     - the synthetic platform needs no diff logic;
     - a new client simply starts with an empty shadow.
   - The trait shape is step 1's design note, approved before code
     (see the back brief).
   - It must keep capture cost proportional to the damaged area, not
     the screen, and it must let the server wait for damage without
     polling.
5. **A scrub catches missed damage.**
   - Every scrub tick, andris re-reads a slice of the screen and
     treats any difference from the shadow as damage. The slices
     cycle so that the whole screen is covered every `SCRUB_PERIOD`,
     4 s.
   - It is x11spice's safety net in andris's own form (survey
     finding 4). It costs one small capture a tick when the screen
     is still.
   - The synthetic platform gains a way to change pixels without
     reporting damage, so that the scrub has a test.
6. **Merged rectangles, within limits.**
   - Changed tiles are merged into rectangles: horizontal runs
     within a tile row, then runs with the same span in adjacent
     rows.
   - At most `MAX_RECTS_PER_FRAME`, 32, are sent per frame. Over
     that, each tile row's changes collapse to one span, and if
     still over, to whole 64-row bands.
   - A rectangle over 4 MiB raw is split by rows, as bands were, to
     stay under kerbside's 16 MiB message cap.
   - The cap bounds ryll's per-message costs (survey finding 11)
     until ryll#510 is fixed.
7. **Under backpressure, merge rather than queue.**
   - A frame starts only when the rate cap allows it and the ack
     window has room (`may_send`).
   - Damage that arrives meanwhile is unioned into the pending
     region.
   - When the frame starts it captures fresh pixels, so stale
     intermediate states are never sent.
   - Within a frame, messages still wait for room, as now.
8. **`MARK` after every frame**, so ryll's FPS measures andris
   (survey findings 6 and 16). A frame that turns out to have no
   changed tiles sends nothing, not even a `MARK`.
9. **JPEG for photographic rectangles, then a lossless refresh.**
   - **When JPEG is allowed:**
     - the client's preference is unset, `auto-glz` or `auto-lz`
       (the modes that leave the choice to the server);
     - and `--jpeg-quality` (new, default 85, 0 to disable) is not 0.
   - An explicit choice, such as `lz4`, or `OFF`, means lossless only.
   - **The classifier** is chosen by step 5 from measurements on a
     corpus of real screen content.
     - The candidate signal is the LZ4 ratio, with a minimum area
       below which JPEG is never worth it.
     - Thresholds are recorded in the Result section with the data
       behind them.
   - **The lossless refresh.** A rectangle sent as JPEG is
     remembered as lossy. Once no damage has touched it for
     `LOSSLESS_DELAY`, 500 ms, it is sent again losslessly, from the
     shadow. The shadow always holds the true pixels, never the
     decoded JPEG, so a still screen ends up exact.
   - `JPEG_ALPHA` is never sent (survey finding 8).
10. **Metrics are logged and exposed in-process.**
    - Each session keeps:
      - frames and draws sent;
      - bytes out per channel;
      - capture and encode time;
      - draws and bytes by encoding: LZ4, JPEG and raw;
      - damaged versus sent area;
      - ack lag: from sending the message that completes a window to
        the `ACK` for it;
      - the ping round trip, latest and mean.
    - They are logged at info every 10 s while frames flow, and once
      at session end. Tests read them through `ServerHandle`.
    - No network endpoint is added, which keeps phase 4's exposure
      rules intact. Phase 10 can revisit this if a soak needs one.
11. **Scroll detection is not in this phase.**
    - Scrolling a full-screen terminal damages most of the screen
      every frame. With SHM, LZ4 and the 30 fps cap, step 9 measures
      whether that sustains 30 fps on the LAN.
    - If it does not, the Result section records the profile, and
      the master plan's Future work gains `COPY_BITS` scroll
      detection, which ryll already renders, sized from that profile.
    - Growing this pull request to chase the number would bury the
      measurement that justifies the work.
12. **Testing at three levels.**
    - **In-process tests (`make test`).** These cover:
      - the update pipeline on its own: tile comparison, merging,
        caps, splitting, JPEG choice and refresh;
      - the display channel against the synthetic platform, with
        every draw decoded and the client-side surface compared to
        the desktop pixel for pixel, as in phase 4.
    - **`make e2e`.** After the first frame, an idle Xvfb produces no
      `surface_drawn` events for 2 s. A small change to the
      keylogger window arrives in a screenshot within 2 s, in fewer
      draws than the screen has bands.
    - **By hand (step 9).** CPU and frame rate, recorded here.
13. **Dependencies.**
    - x11rb gains the `damage` and `shm` features, and `memmap2` is
      added.
    - `cargo deny` must pass, and no new advisory ignore is expected.

## Execution

Every step is a commit on the andris branch
`x11-desktop-phase-05-damage`, in a worktree at
`/srv/kasm_profiles/mikal/vscode/src/shakenfist/andris-wt-p05`. The
management session makes each commit after reviewing the sub-agent's
diff.

- Every commit must leave `make lint` and `make test` passing.
- Cargo runs only through the `Makefile`, never on the host.
- No code is translated from spice-server, spice-gtk or x11spice.
  Their behaviour is cited from the survey.
- Every step adds tests for what it adds.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Complete | Commit the phase 4 close-out and this plan. See brief 0. |
| 1 | high | opus | none | Complete | The damage-aware capture seam and the synthetic platform's damage. Gated. See brief 1. |
| 2 | high | opus | worktree | Complete | The X11 backend: XDamage, MIT-SHM through a memfd, and the `GetImage` fallback. See brief 2. |
| 3 | high | opus | none | Complete | The update pipeline: shadow, tile comparison, merged rectangles, limits. See brief 3. |
| 4 | high | opus | none | Complete | The display channel: damage-driven frames, rate cap, backpressure, scrub, `MARK`. See brief 4. |
| 5 | high | opus | none | Complete | JPEG: the corpus, the classifier, the lossless refresh, `--jpeg-quality`. See brief 5. |
| 6 | medium | sonnet | none | Not started | Per-session metrics. See brief 6. |
| 7 | medium | sonnet | none | Complete | `make e2e`: idle sends nothing, a small change sends a little. See brief 7. |
| 8 | medium | sonnet | none | Not started | Documentation. See brief 8. |
| 9 | medium | management | none | Not started | Measurements: idle CPU and scrolling frame rate. See brief 9. |
| 10 | medium | management | none | Not started | Push audit, pull request, queue. See brief 10. |

Step 1 is gated because the seam's shape is cheap to propose and
expensive to change once steps 2 to 5 sit on it. Step 2 runs in a
worktree because it brings andris's first `unsafe` code, which the
management session reviews on its own before merging it in.

In every brief, A and R abbreviate as in the survey. Ryll paths are
read at the pinned revision.

**Brief 0: land the plan** (management session).

- Commit the phase 4 close-out, then this plan, on this branch. Do
  not open a pull request yet; the branch becomes this phase's one
  pull request in step 10.

**Brief 1: the damage-aware capture seam.**

Before writing code, reply with a design note of no more than a page
for the management session to approve. Wait for approval before
writing code. The note covers:

- the trait or traits replacing `FrameSource` (A
  `andris/src/platform/mod.rs:313-317`);
- the rectangle and region types, and where they live;
- how the server waits for damage without polling. The platform's
  other seams use `watch` receivers (`CursorSource`) or blocking
  calls run on the blocking pool;
- how a resize is reported;
- how the synthetic platform reports damage, and how it changes
  pixels without reporting damage, for decision 5's test.

Constraints:

- **Decision 4.** The platform accumulates damage and captures
  rectangles on request. The server owns the shadow.
- **Capture cost.** It must be proportional to the rectangles asked
  for, not the screen. A capture returns pixels for each rectangle
  requested, or one buffer with the rectangles' positions; the note
  says which.
- **A complete capture.** It stays possible, for a new client and
  after a resize.
- **Region accounting.** Damage taken by the server is cleared in
  the platform, atomically with the take, so no damage reported
  between take and capture is lost. The note says how.
- **The synthetic platform.**
  - `advance()` (A `andris/src/platform/synthetic.rs:117-126`)
    reports the square's old and new rectangles as damage.
  - `set_size` reports the whole screen.
  - A new method changes pixels with no damage.
- **Gone.** Keep `platform::Gone` semantics: a desktop that has gone
  is `Gone`, and a transient failure is not.

After approval:

- implement the seam and the synthetic side;
- adapt the X11 backend minimally so that it still builds, treating
  every request as a full-screen `GetImage` for now. Step 2 replaces
  it;
- adapt `display.rs` only as far as needed to keep today's behaviour
  and tests passing on the new seam.

**Brief 2: the X11 backend.**

- **Dependencies.**
  - Enable x11rb's `damage` and `shm` features (A
    `andris/Cargo.toml:30`).
  - Add `memmap2`, then run `make lock` and keep only the new
    lockfile entries.
- **Startup checks.** Add DAMAGE to the checks in
  `andris/src/platform/x11/checks.rs`: it is required, with a clear
  error. MIT-SHM is optional.
- **Damage.**
  - Create a `RawRectangles` damage object on the root, on the
    capture connection or a dedicated one; the design note from step
    1 says which.
  - Accumulate `DamageNotify` areas into the step 1 region.
  - Watch for the damage object's whole-screen notifications after a
    resize.
  - Do not call `DamageSubtract` per event. With `RawRectangles`
    there is nothing to subtract.
- **MIT-SHM** (decision 3).
  - Use `memfd_create` from `libc`, size the file with `set_len`,
    map it with `memmap2`, and attach it with `shm::attach_fd`.
  - Size the segment for the whole screen, and recreate it on
    resize.
  - `ShmGetImage` each requested rectangle at offset 0 of the
    segment, `Z_PIXMAP`, plane mask `!0`. Copy the rectangle's rows
    out before the next request. Rows are `width * 4` bytes, as
    `capture.rs:50-57` explains for `GetImage`.
  - `unsafe` is confined to `andris/src/platform/x11/shm.rs`.
    - Add `#![deny(unsafe_code)]` to `andris/src/lib.rs`, and
      `#[allow(unsafe_code)]` only on that module.
    - Every block gets a `// SAFETY:` comment that names the
      invariant: the X server writes the segment only while serving
      a request, and andris reads it only after the reply.
- **The fallback** is `GetImage` per rectangle when MIT-SHM is absent
  or below 1.2, the attach fails, or the display is TCP. Log at info,
  once, which path is in use.
- **Tests.** The devcontainer test run has no X server.
  - Unit-test what can be tested without X: the region
    accumulation, and the arithmetic of rows and offsets.
  - Add an X11 integration test that `make e2e` runs against its
    Xvfb, gated on an environment variable such as
    `ANDRIS_TEST_DISPLAY`, so `make test` skips it. It should:
    - draw a rectangle with a core `PolyFillRectangle` on the root;
    - see damage covering it;
    - capture that rectangle through SHM and through the fallback;
    - check that the pixels match.
- Do not read x11spice's `display.c` for more than behaviour.

**Brief 3: the update pipeline.**

Replace `andris/src/server/frames.rs`'s whole-frame bands with an
update pipeline. It knows nothing of sockets, as `frames.rs` does
not.

- **The shadow** is the last-sent pixels for the whole surface,
  created at the surface's size and dropped on resize.
- **Comparison.** Given the captured rectangles, compare each 64x64
  tile they cover with the shadow. Edge tiles are partial. Copy the
  changed tiles into the shadow.
- **Merging and limits** follow decision 6:
  - merge horizontal runs in a tile row, then equal spans in
    adjacent rows;
  - cap at `MAX_RECTS_PER_FRAME` = 32, collapsing to one span per
    tile row and then to 64-row bands;
  - split any rectangle over 4 MiB raw by rows.
- **Encoding.** Each rectangle becomes one `DRAW_COPY` built as
  `encode_band` builds one today (A `frames.rs:143-163`), at the
  rectangle's position. Set `left` from the rectangle, where bands
  always used 0. The image's width and height are the rectangle's.
  It is LZ4 if allowed and smaller, otherwise a raw bitmap. Step 5
  adds JPEG through a hook left here.
- Keep the image id counter (`FrameEncoder::next_image_id`).
- **Tests.**
  - Every merge case.
  - The caps.
  - The split.
  - That a tile change at each edge and corner is sent.
  - That an unchanged damaged area sends nothing.
  - That decoding every `DRAW_COPY` onto a client-side surface
    reproduces the frame exactly. Reuse phase 4's decode helpers in
    `frames.rs`'s tests and `andris/tests/display.rs`.

**Brief 4: the display channel.**

Rework `andris/src/server/display.rs`'s loop (A `:94-112`). In one
`select!` it:

- reads client messages;
- waits for damage through step 1's mechanism;
- runs a rate-cap timer: a frame may start at most every
  `frame_interval`;
- runs the scrub timer (decision 5).

Constraints:

- **A frame starts only when** damage is pending, the cap allows
  it, and `conn.may_send()`. Damage arriving otherwise stays pending
  in the region (decision 7). Never queue pixels.
- **A frame** takes the pending region, captures it, runs step 3's
  pipeline, sends the draws, then sends `MARK` if it sent any draw
  (decision 8).
- **The scrub** captures one slice per tick and feeds differences
  in as damage. It cycles so that the whole screen is covered every
  `SCRUB_PERIOD` (4 s). A slice is a band of rows; derive its height
  from the screen height and the tick.
- **A resize**, reported by the platform or seen as a capture of
  another size, re-runs the start sequence as now (A `:248-270`).
- **Keep:**
  - the start sequence;
  - the capture-failure handling (A `:215-247`);
  - the `PREFERRED_COMPRESSION` handling.
- **`--frame-interval-ms`.** The default becomes 33 and the minimum
  stays 20. Update the help text and `andris/src/config.rs`'s test
  (A `config.rs:374`).
- **Tests, in `andris/tests/display.rs`:**
  - an idle synthetic desktop sends no `DRAW_COPY` after the first
    frame for 1 s;
  - `advance()` sends draws covering only the square's old and new
    places, followed by a `MARK`;
  - with acks withheld, many `advance()`s produce one merged frame
    once acks resume, and the client's surface then matches;
  - a change made without damage arrives within `SCRUB_PERIOD`
    plus slack. Make the period injectable, as `frame_interval` is,
    so the test is fast;
  - the rate cap holds under continuous damage.

**Brief 5: JPEG.**

- **The corpus.** Build a small tool, under `tools/` or as an
  `#[ignore]`d test, that reads PNG or raw BGRX captures and reports
  each tile-merged rectangle's raw size, LZ4 size, JPEG size at
  quality 85, and encode times.
  - Run it on captures of `:10` taken with `xwd` by the management
    session: a terminal, a browser on a text page, a browser on a
    photo, and a desktop background.
  - **These captures are the operator's desktop.** Never commit
    them, and never put them in a log or a pull request. Commit only
    the numbers, in the Result section.
  - Tests use generated content: smooth noise and gradients for
    "photographic", and glyph-like hard edges on a flat background
    for "text".
- **The classifier.** Choose it from the numbers. The starting
  hypothesis is: JPEG when the rectangle is at least 64x64 and LZ4
  saves less than half. Record the chosen thresholds and why. JPEG
  is used only if it is smaller than the LZ4 or raw choice.
- **Permission** follows decision 9: the client's preference is
  unset, `auto-glz` or `auto-lz`, and `--jpeg-quality` is not 0.
  Add `--jpeg-quality` to `andris/src/config.rs`, range 0 to 100,
  default 85.
- **The lossless refresh** follows decision 9.
  - Track the rectangles last sent lossy.
  - A lossy area untouched by damage for `LOSSLESS_DELAY` (500 ms)
    is sent again losslessly from the shadow, as its own frame with
    its own `MARK`.
  - Damage touching a lossy area resets its timer.
- **Tests.**
  - A photographic rectangle under `auto-glz` arrives as JPEG.
  - Under `lz4`, `OFF` or `--jpeg-quality 0`, it arrives as LZ4 or
    raw.
  - Text-like content never arrives as JPEG.
  - After `LOSSLESS_DELAY` of stillness, the client's surface equals
    the desktop exactly.

**Brief 6: per-session metrics.**

- **The stats** are decision 10's list, held per session: a struct
  behind the session, updated by each channel.
- **Ack lag.**
  - Record the send time of the message that completes each ack
    window, in `andris/src/server/channel.rs`'s `AckWindow` (A
    `:140-190`).
  - Take the lag when that window's `ACK` arrives.
- **Ping round trip.** Take it from the existing `PONG` handling (A
  `channel.rs:354-361`). Keep the debug line.
- **Logging.** Log a one-line summary at info every 10 s while
  frames flow, and once when the session ends. Do not log at all
  while nothing is sent.
- **Access.** Expose a snapshot through `ServerHandle::session()` for
  tests.
- **Tests.** The counters move as frames are sent, and ack lag and
  the ping round trip are recorded.

**Brief 7: `make e2e`.**

- Extend `scripts/e2e-drive.py` to subscribe to the control socket's
  `surface_drawn` events after the first screenshot (R
  `ryll/src/control/server.rs:694-760`). The method is `subscribe`,
  with the same JSON framing the script already uses.
- **Idle.** No `surface_drawn` for 2 s while nothing changes.
- **A small change.** Have the keylogger repaint a small part of its
  window. For example, a new method recolours a 64x64 square when it
  receives a particular key. Then assert that:
  - a screenshot shows the change within 2 s;
  - it took fewer `surface_drawn` events than the screen has 64-row
    bands.
- Run step 2's gated X11 integration test from `make e2e` against the
  same Xvfb.
- Keep the e2e job's time near today's 5 s warm. Record it.

**Brief 8: documentation.**

- **`docs/running.md`:**
  - what `--frame-interval-ms` now means, and its new default;
  - `--jpeg-quality`, and how the client's compression preference
    interacts with it;
  - the metrics log line;
  - MIT-SHM and its fallback, and what to check when the log says
    `GetImage` is in use.
- **`ARCHITECTURE.md`:** the capture path, from damage through
  shadow comparison to rectangles.
- **Check.** `grep -rn "phase" docs/running.md` still finds nothing.

**Brief 9: measurements** (management session).

- **Idle CPU on Xvfb.** At 2212x1308, with ryll headless connected
  and nothing changing, run `pidstat` or `/proc/<pid>/stat` sampling
  over 60 s for andris and Xvfb.
- **Idle CPU on `:10`.**
  - Run andris against `:10` on loopback, with ryll headless
    connected and sending no input, for 60 s, with the operator not
    typing.
  - Record the figure, and the scrub's share of it from the metrics.
- **Scrolling.**
  - Use a full-screen `xterm` on Xvfb at 2212x1308, running
    `seq 1 10000000`, or a slower scroll at the terminal's natural
    rate.
  - Ryll GUI connects on the LAN interface, not loopback. Read its
    FPS, then andris's frames per second, capture time, encode time
    and bytes per second from the metrics log.
- **Record** all of it in the Result section, with the 2% and 30 fps
  targets beside the numbers. If scrolling falls short, apply
  decision 11.

**Brief 10: push audit, pull request, queue** (management session).

- Run `PUSH-AUDIT.md` over the branch, as phase 4 did. Fix or
  decline each finding in the Result section.
- Open the pull request only when the operator asks.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| XDamage misses changes on `:10`: GL clients, Present flips, or xorgxrdp quirks. | Decision 5's scrub bounds staleness to `SCRUB_PERIOD`. Brief 4 tests a change made with no damage. Step 9 watches the metrics on `:10` for scrub-found differences, and records the count. |
| MIT-SHM is unusable where andris runs: another IPC namespace, a TCP display, or a restricted container. | The memfd and `ShmAttachFd` path avoids SysV IPC. The `GetImage` fallback is logged at startup. Brief 2's integration test exercises both paths. |
| The first `unsafe` code is unsound. | It is confined to one module under `#![deny(unsafe_code)]`, with a `SAFETY:` comment on each block. It is reviewed in its own worktree (step 2) and again in the push audit. Data is copied out only after the reply. |
| JPEG smears text, or a lossy area stays lossy. | The classifier is chosen from measurements and tested to reject text-like content. The lossless refresh makes a still screen exact, and a test proves it. `--jpeg-quality 0` turns JPEG off. |
| A `MARK` per frame confuses a client other than ryll. | spice-gtk sets an already-set flag (survey finding 16). Other clients are best effort under the master plan. |
| Many small rectangles hit ryll#510's quadratic cost. | Decision 6's cap of 32 per frame. |
| 30 fps scrolling is out of reach without `COPY_BITS`. | Step 9 measures it, and decision 11 routes the outcome to Future work rather than into this pull request. |
| A 30 fps cap raises CPU under constant animation, compared with phase 4's 5 fps. | That is the intent for a responsive desktop. The metrics make the cost visible, and the option still lowers the cap. |
| Damage accounting loses an update between take and capture. | Brief 1's design note must say how take and clear are atomic. The scrub is the backstop. |

## Definition of done

- **The checks pass.** `make lint test e2e` passes. The pull request
  passed its smoke tier, and `merge-build`, "End to end" and `Can
  merge` in the merge queue.
- **Unsafe is confined.** `grep -rnE 'unsafe (\{|fn|impl)'
  andris/src` matches only in `andris/src/platform/x11/shm.rs`.
  `grep -n 'deny(unsafe_code)' andris/src/lib.rs` matches.
- **Idle sends nothing.** A test named for it passes in-process, and
  `make e2e` asserts no `surface_drawn` events for 2 s at idle.
- **Missed damage is caught.** A test changes pixels without damage
  and sees them arrive.
- **The client ends up exact.** A test shows the client's surface
  equal to the desktop after a lossy update and `LOSSLESS_DELAY`.
- **`MARK` per frame.** A test sees a `MARK` after each frame that
  sent draws, and none after an empty one.
- **Options.** `andris --help` lists `--jpeg-quality`, and
  `--frame-interval-ms` defaults to 33.
- **Measured.** The Result section records:
  - idle CPU on Xvfb at 2212x1308, and on `:10`, against the 2%
    target;
  - the scrolling frame rate against 30 fps;
  - the classifier's thresholds and the corpus numbers behind them;
  - the e2e job's time.
- **Documentation.**
  - `docs/running.md` describes `--frame-interval-ms` as a cap,
    `--jpeg-quality`, and the metrics line.
  - `grep -rn "phase" docs/running.md` finds nothing.
- **The ryll gaps are tracked.** ryll#510 and ryll#512 are open, or
  closed by ryll.

## Result

Not yet started.

## Appendix: the damage spike

The spike behind survey finding 3 counts the damage events on a
display's root window. Run it with python-xlib in a virtualenv:
`python damage_spike.py :10 10`.

```python
"""Count XDamage raw rectangles on a display's root window for N seconds."""
import sys
import time

from Xlib import display
from Xlib.ext import damage

name, seconds = sys.argv[1], float(sys.argv[2])
d = display.Display(name)
root = d.screen().root
geom = root.get_geometry()
# DAMAGE requires QueryVersion before any other request.
d.damage_query_version()
root.damage_create(damage.DamageReportRawRectangles)
event_type = d.extension_event.DamageNotify
count = 0
area = 0
end = time.time() + seconds
while time.time() < end:
    while d.pending_events():
        ev = d.next_event()
        if ev.type == event_type:
            count += 1
            area += ev.area.width * ev.area.height
    time.sleep(0.01)
print('events per s', count / seconds,
      'screen fraction per s', area / seconds / (geom.width * geom.height))
```

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.

Step 1 is gated beyond that. Its sub-agent returns a design note for
the capture seam before writing code, and the management session
approves it or sends it back before the step continues. The seam is
cheap to propose and expensive to change once the X11 backend, the
update pipeline, the display loop and JPEG are built on it.
