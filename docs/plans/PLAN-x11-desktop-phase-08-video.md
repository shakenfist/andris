# Phase 8: Video streams

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: high, as the master plan asks. This phase turns on
encoding heuristics (which areas become streams, and when they stop),
on SPICE stream semantics (multimedia time, reports), and on a ryll
change to its shared crates and its client defaults. Two choices were
put to the operator before the plan was written: how ryll and andris
agree on H.264 (decision 1), and how openh264 is built given H.264's
patents (decision 2).

## Scope

In scope, as one ryll pull request followed by one andris pull request:

- **Ryll first** (decision 1, step 2):
  - its openh264 encoder moves to `shakenfist-spice-compression`
    behind the `encode` feature, where andris may use it;
  - ryll advertises `CODEC_H264` by default, with MJPEG first in its
    codec preference;
  - the stream-compositing bug in survey finding 8 is fixed;
  - headless mode gains a control-socket `stats` verb that reports its
    stream counters (ryll#512), so e2e can assert on streams.
- **Stream detection** in andris's display pipeline: an area of the
  desktop that keeps changing, and looks like a photograph, becomes a
  stream (decisions 3 and 4).
- **Serving streams:**
  - `STREAM_CREATE`, `STREAM_DATA`, `STREAM_ACTIVATE_REPORT`,
    `STREAM_DESTROY` and `STREAM_DESTROY_ALL`;
  - H.264 from openh264, built from source with nasm (decision 2);
  - multimedia time on each frame (decision 6).
- **Teardown:** a stream ends when its area goes quiet, and the area
  is then repaired losslessly (decision 5).
- **Rate control** from the client's `STREAM_REPORT` and from
  andris's own flow control (decision 7).
- **Checks:** unit and integration tests on the synthetic platform,
  `make e2e` with a moving picture in Xvfb, and one check in
  `make e2e-xrdp`.
- **Documentation**, including the patent position, and operator
  acceptance on `:10` with a video playing on the desktop.

Out of scope:

- **MJPEG, VP8 and other codecs** (decision 1). Andris encodes only
  H.264. A client without `CODEC_H264` gets today's JPEG draws.
- **Cisco's openh264 binary through `libloading`** (decision 2). It
  becomes an open question for phase 10, where the PyPI wheels decide
  what ships.
- **More than one stream per display channel** (decision 4). This
  goes to Future work.
- **Whole-screen H.264.** xrdp and gnome-remote-desktop encode the
  whole monitor. That is a different design, and it blurs text, which
  SPICE avoids by streaming only photographic areas. Not this phase.
- **Hardware encode (VA-API).** Already in Future work.
- **Audio, and audio/video sync.** Audio is phase 9. Andris sends
  frames with multimedia time, and phase 9 decides how audio uses the
  same clock.
- **Making openh264 optional in ryll's crates** (ryll#499). That issue
  stands; this phase only moves the encoder.

## What the survey found

Three surveys ran: ryll at `origin/develop` (1a28a36), andris at
develop (`b7ba2dd`), and the upstream sources (spice-protocol
0.14.5, spice-common, spice-server v0.16.0-50, spice-gtk,
openh264-rs 0.9.8, xrdp devel, gnome-remote-desktop).

1. **The master plan's phase 8 section holds as far as it goes**,
   but leaves out what makes the phase work. It says nothing about:
   - where the encoder lives (finding 4);
   - ryll's refusal to accept H.264 by default (finding 5);
   - multimedia time (finding 9);
   - how a test sees a stream (finding 10).

   This plan adds them, and the master plan's section is corrected to
   point here.
2. **Wire types exist, with writers, for every stream message**
   (`shakenfist-spice-protocol/src/messages/display.rs`):
   - `StreamCreate` :1293, `StreamData` :1378, `StreamDataSized`
     :1433, `StreamClip` :1499, `StreamDestroy` :1521,
     `StreamActivateReport` :1538 and the client's `StreamReport`
     :1604, all round-trip tested;
   - `STREAM_DESTROY_ALL` has an empty body and no type;
   - opcodes are in `constants.rs:311-337`;
   - all are named in `logging::message_names`, so kerbside's
     allowlist (`kerbside-proxy/src/allowlist.rs:120`) passes them
     already.
3. **Andris has no stream path at all:**
   - `DisplayOpcode` (`andris/src/server/opcodes.rs:101-112`) has no
     `STREAM_*` variant;
   - `STREAM_REPORT` and `PREFERRED_VIDEO_CODEC_TYPE` are parsed and
     ignored (`andris/src/server/display.rs:309-310`, `:341-349`);
   - andris already offers `STREAM_REPORT` and `PREF_VIDEO_CODEC_TYPE`
     in its link reply (`andris/src/server/link.rs:96-99`).
4. **The only H.264 encoder is in the wrong crate.** Ryll's
   `H264Encoder` is in `shakenfist-spice-renderer/src/encoder/h264.rs`
   (:134-366), which andris must not depend on (`AGENTS.md`).
   - Configuration: `ScreenContentRealTime`, Baseline profile, a
     bitrate from pixels × fps clamped to 1–20 Mbit/s, forced IDR on
     request, and even dimensions required.
   - `shakenfist-spice-compression` encodes only LZ4 and JPEG
     (`src/encode.rs`), but already depends on openh264 0.9.8
     unconditionally, for its decoder (`src/video.rs`).
5. **Ryll accepts H.264 only when started with `--experimental-h264`**
   (`link.rs:724-730`, `ryll/src/config.rs:282-289`).
   - Without the flag its display caps are `DEFAULT_DISPLAY`, with
     `MULTI_CODEC` and `CODEC_MJPEG` but no `CODEC_H264`, and its
     codec preference is `[MJPEG]`.
   - The flag was introduced because spice-server's GStreamer H.264
     never decoded in ryll (ryll#398: x264 intra-refresh without
     repeated parameter sets, or AVCC framing).
   - openh264's Annex B output repeats SPS and PPS on each IDR, and
     ryll's own test round-trips it (`video.rs:1082`).
6. **Spice-server picks a stream's codec in the client's preference
   order**, from the codecs the client has caps for (`server/dcc.cpp`,
   `dcc_create_video_encoder`).
   - A ryll that advertises `CODEC_H264` but lists MJPEG first
     therefore still gets MJPEG from spice-server.
   - That is why decision 1 is safe for ryll's QEMU users.
7. **Kerbside forwards the client's display caps to the backend
   unchanged** (`kerbside-proxy/src/backend.rs:93-94`). Through
   kerbside, andris sees ryll's `CODEC_H264` exactly as it would
   directly.
8. **Ryll composites stream frames simply, and has one bug:**
   - it ignores the clip, the `TOP_DOWN` flag and scaling, and paints
     each decoded frame at the dest rect's corner as soon as it
     arrives;
   - it clips the frame's width to the dest's but keeps the frame's
     stride, and `DisplaySurface::blit` uses `width * 4` as the
     source stride (`display/surface.rs:78`;
     `channels/display.rs:1873-1949`), so a frame wider than its dest
     paints sheared.

   Andris must send frames exactly the dest's size, and the ryll step
   fixes the stride anyway.
9. **Multimedia time is how a client judges lateness:**
   - ryll counts a frame as dropped in its `STREAM_REPORT` when the
     frame's `multi_media_time` is behind the client's clock, which
     is the server's `MAIN_INIT` time plus the client's elapsed time
     (`channels/display.rs:1815-1871`, `mm_clock.rs`);
   - spice-gtk decodes H.264 regardless, but schedules display at the
     frame's time, and drops late MJPEG before decoding;
   - andris's only multimedia time today is `MAIN_INIT`'s
     `session.elapsed()` in milliseconds (`main_channel.rs:113`).

   A frame stamped with its capture time arrives late by its
   encode-and-transport delay, so every frame would count as dropped
   (decision 6).
10. **Headless ryll exposes no stream counters** (ryll#512).
    - Its control socket's `status` reports only connection, agent
      and surfaces, and no verb returns `DisplaySnapshot`.
    - It does log `display: stream_create: id=…, codec=…` and
      `display: stream_destroy … decoded_ok=…` at INFO
      (`channels/display.rs:1635`, `:1092-1104`).
    - A screenshot shows stream frames, because the headless mirror
      applies them; `docs/multi-mode-parity.md:90-93` says otherwise
      and is stale.
11. **openh264 builds offline, and nasm matters:**
    - the crate vendors Cisco's source (BSD-2-Clause, allowed by
      `deny.toml:54`), and its build compiles it with `cc`;
    - without `nasm` the assembly is silently skipped;
    - andris's devcontainer image has no nasm
      (`.devcontainer/Dockerfile:9-13`).

    Measured in that image for a 1080p screen-content frame, including
    the BGRA to I420 conversion: about 25 ms without nasm and 10.5 ms
    with it. Both images are built through `make`, so CI follows the
    image. The cold build takes about 8 s.
12. **The openh264 encoder API covers what rate control needs**
    (`openh264/src/encoder.rs`):
    - `force_intra_frame()`, verified;
    - `RateControlMode::Bitrate`, `max_frame_rate`,
      `skip_frames(true)` (a skipped frame yields no NAL units, and
      must not be sent as an empty frame), and `intra_frame_period`;
    - BT.601 limited-range BGRA to I420 conversion;
    - Annex B output with 4-byte start codes.

    `YUVBuffer::new` panics on an odd width or height. Whether the
    target bitrate can change without re-creating the encoder is not
    established; step 1 finds out.
13. **Andris's pipeline has the pieces a detector needs, but no
    history:**
    - damage is a per-connection set of dirty 64×64 tiles with no
      timestamps (`platform/damage.rs`);
    - `Display::frame()` (`display.rs:458-493`) takes it at most once
      per `--frame-interval-ms` (33 ms by default);
    - `choose_encoding` (`updates/encode.rs:206-250`) already decides
      "photographic" by how badly LZ4 compresses a rectangle;
    - `LossyTiles` (`updates/lossy.rs`) already repairs JPEG tiles
      losslessly 500 ms after they settle.

    A detector keeps its own per-tile history over the frames
    `frame()` sends (decision 3).
14. **What spice-server does, as behaviour to learn from and not code
    to copy** (`server/video-stream.h`, `video-stream.cpp`,
    `dcc-send.cpp`, `gstreamer-encoder.c`):
    - a stream starts after 20 frames of the same drawable box, each
      within 200 ms of the last, of which at least 20% are "gradual"
      (photographic);
    - a stream must be at least 96×96;
    - a stream stops after 1 s without a frame, and the area is then
      sent losslessly;
    - stream reports use a window of 5 frames and 1 s;
    - bitrate starts at 10 Mbit/s (2.5 on slow links), halves on drops,
      and climbs slowly when reports are clean;
    - since 0.16.0 frames carry the server's raw clock.
15. **The e2e image has no video player** and no xdotool. It has
    python3 and python3-xlib, which `e2e-keylogger.py` already paints
    with, so a moving picture can be generated without new packages
    (decision 8).
16. **Andris's documentation says there is no video.** The places are
    `README.md:22`, `docs/index.md:14`, `docs/running.md:275`, `:350`
    and `:416-418`, and `ARCHITECTURE.md:139`. `ARCHITECTURE.md:144-145`
    also says "Images are LZ4 or uncompressed", which has been stale
    since phase 5 added JPEG.

The master plan's phase 8 section and its `index.md` row are corrected
in this plan's commit.

## Decisions

1. **H.264 only, agreed through ryll's caps.** This is the operator's
   choice.
   - Ryll advertises `CODEC_H264` by default and lists MJPEG first in
     `PREFERRED_VIDEO_CODEC_TYPE`. `--experimental-h264` now means
     "prefer H.264", putting it first.
   - Spice-server sorts by the client's list (finding 6), so ryll's
     QEMU users keep getting MJPEG and ryll#398 stays avoided.
   - Andris encodes only H.264, so it uses H.264 whenever the client
     has `MULTI_CODEC` and `CODEC_H264`, whatever the order. Without
     those caps it sends no streams.
   - MJPEG was declined: an MJPEG stream costs about what today's JPEG
     draws do, so it would add a code path for little gain.
2. **openh264 from source, with nasm; patents are decided in phase
   10.** This is the operator's choice.
   - Andris builds openh264 from its vendored source, as ryll already
     ships it, and `.devcontainer/Dockerfile` gains `nasm`.
   - `docs/running.md` states the position: the code is BSD-2-Clause,
     but a build from source carries none of Cisco's patent coverage,
     which applies only to Cisco's own binary downloaded onto the
     user's machine.
   - The master plan's open question 7, added with this plan, leaves
     phase 10 to decide: ship the compiled encoder in wheels, or build
     with `libloading` and have the operator fetch Cisco's binary.
3. **Detection works on tiles over frames.** It runs in `server/`,
   over what `Display::frame()` sends. `platform/` stays free of
   SPICE notions.
   - **Heat.** A tile is *hot* when it changed in at least 20 of the
     frames sent in the last 2 s, with no gap over 200 ms between
     changes. These are spice-server's numbers re-expressed in tiles,
     because andris has tiles rather than drawables.
   - **Candidate.** A candidate is the bounding box of a 4-connected
     cluster of hot tiles. It must cover at least 2 × 2 tiles
     (128×128, above spice-server's 96×96 minimum).
   - **Photographic filter.** At least 20% of the candidate's recent
     draws must have been JPEG, using `choose_encoding`'s existing
     test, so that continuous non-photographic change does not stream:
     - a scrolling terminal;
     - a build log;
     - an animated progress bar.

     This is the decision most likely to be argued with: it means a
     video with a client that disables JPEG never streams. That is
     deliberate, since such a client asked for lossless pixels.
   - **Size.** The stream's rectangle is the tile-aligned bounding
     box, clamped to the surface and then rounded down to even
     dimensions, so openh264 never sees an odd size. A pixel row or
     column left outside stays on the draw path.
4. **One stream per display channel.**
   - The largest qualifying candidate wins.
   - While a stream lives, a cluster inside its rectangle feeds it.
   - A cluster that outgrows the rectangle by more than its area
     (spice-server allows twice) ends the stream, and detection starts
     afresh. That covers a video window being moved or resized.
   - More than one stream goes to Future work: the use case is one
     video on a desktop, and one stream keeps rate control and
     teardown simple.
5. **Stream frames replace draws inside the rectangle.**
   - **Frames.** On each frame tick, damage inside the stream's
     rectangle is not encoded as draws. If any tile there is dirty,
     the whole rectangle is captured, encoded and sent as one
     `STREAM_DATA`. Damage outside it takes the normal draw path.
   - **Shadow.** Stream frames update the shadow, as draws do, so the
     scrub does not find false differences.
   - **Lossy tiles.** They mark their tiles lossy in `LossyTiles`.
   - **Teardown.** When no tile in the rectangle has changed for 1 s,
     andris sends `STREAM_DESTROY` and makes the rectangle's lossy
     tiles due at once, so the existing lossless refresh repairs the
     area. A client therefore never keeps H.264's blurred text once
     the video stops.
   - **Other endings.** A surface replacement (resize) sends
     `STREAM_DESTROY_ALL` before `SURFACE_DESTROY`. An encoder
     failure, or a report saying the client cannot decode
     (`num_frames == 0` and `num_drops == u32::MAX`), destroys the
     stream and disables streams for that connection, with one
     warning.
6. **Frames are stamped ahead of capture by a fixed latency budget.**
   - Each frame's `multi_media_time` is the session's elapsed
     milliseconds at capture plus `STREAM_LATENCY_BUDGET`, 80 ms.
   - A client then counts a frame as dropped only when the pipeline
     took longer than the budget, and its report's `last_frame_delay`
     is the headroom left. That is the signal rate control needs.
   - The cost is up to 80 ms of display latency on spice-gtk clients,
     which present at the frame's time. Ryll paints frames on arrival
     and pays nothing.
   - No `MAIN_MULTI_MEDIA_TIME` is sent: both clocks are monotonic,
     and the drift over a session is far below the budget. Phase 9
     revisits this for audio.
7. **Rate control is simple and visible.**
   - **Start.** The encoder starts at ryll's formula (pixels × fps,
     clamped to 1–20 Mbit/s), in `RateControlMode::Bitrate` with frame
     skipping on, `max_frame_rate` from `--frame-interval-ms`, and no
     periodic IDR (TCP loses nothing; an IDR opens each stream).
   - **Reports.** `STREAM_ACTIVATE_REPORT` asks for a window of 5
     frames and a 1 s timeout, with a random `unique_id`. Andris then
     adjusts on each report:
     - drops, or a negative `last_frame_delay`: halve, but not below
       256 kbit/s;
     - headroom under a quarter of the budget: multiply by 3/4;
     - 2 s of clean reports: raise by 10%, up to the start cap.
   - **Server drops.** When a stream frame is due but the ack window
     is full, the frame is not captured (damage keeps merging, as for
     draws) and counts as a server drop. Two of those within a second
     cut the rate by 3/4.
   - **Logging.** Every change is logged at debug, and the metrics
     line reports the current target.
   - Whether the target can change in place or needs a new encoder
     (with an IDR) is step 1's question.
8. **The e2e video is generated, not played.**
   - A python-xlib script paints a 320×240 area with a moving
     photographic pattern at about 25 fps, plus a static text window.
   - ryll headless's new `stats` verb asserts:
     - a stream was created with codec H.264;
     - frames decoded with no decode failures;
     - the stream was destroyed within 2 s of the script stopping.
   - A screenshot then checks the area exactly against the last frame,
     which proves the lossless repair.
   - A second run with ryll's new `--no-h264`, which drops
     `CODEC_H264`, asserts that no stream is created.
9. **The metrics line gains stream fields**: streams created and
   destroyed, stream frames, stream kbit/s, encode ms and target
   bitrate. `stats.rs`'s exhaustive `ENCODINGS` guard forces this.

## Execution

Implementation is done by sub-agents briefed from this plan. The
management session reviews each diff and makes each commit. For the
ryll step, the rules are ryll's: its `make lint` and `make test`, and
its conventions in its `AGENTS.md`.

- Every andris commit must leave `make lint` and `make test` passing.
- Cargo runs only through the `Makefile`, never on the host.
- No code is translated from spice-server, spice-gtk, x11spice, xrdp,
  gnome-remote-desktop or openh264-rs's examples. Their behaviour is
  cited from the survey; local copies are in `/srv/src-reference/spice/`.
- Every step adds tests for what it adds.
- **No sub-agent connects to, signals, captures or injects into the
  host's `:10` session**, nor touches its xrdp, sesman or chansrv. The
  host shell has `DISPLAY=:10.0`: unset `DISPLAY` and `XAUTHORITY`
  for every X command and make target. All xrdp testing happens in the
  test-bed container, on `:50`.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Complete | Commit the phase 7 close-out and this plan; create the ryll worktree. See brief 0. |
| 1 | high | opus | none | Complete | Spike: openh264 in andris's image with nasm, bitrate changes, `Send`, and the e2e picture source. Gated. See brief 1. |
| 2 | high | opus | none | Complete | Ryll: move the H.264 encoder, advertise H.264, fix the stride, add `stats`. Ryll PR. See brief 2. |
| 3 | medium | sonnet | none | Complete | Andris: bump the ryll pin, add nasm to the image. See brief 3. |
| 4 | high | opus | none | Not started | The stream lifecycle on the display channel: caps, opcodes, encoder, create/data/destroy, reports parsed. See brief 4. |
| 5 | high | opus | none | Not started | Detection, the frame path inside a stream, teardown and lossless repair. See brief 5. |
| 6 | high | opus | none | Not started | Rate control from reports and server drops; metrics fields. See brief 6. |
| 7 | high | opus | none | Not started | End-to-end checks: `make e2e` and one `make e2e-xrdp` check. See brief 7. |
| 8 | medium | sonnet | none | Not started | Documentation. See brief 8. |
| 9 | medium | management | none | Not started | Operator acceptance on `:10`, with a video. See brief 9. |
| 10 | medium | management | none | Not started | Push audit, pull request, queue. See brief 10. |

Steps 1 and 2 may run in parallel, but step 2's encoder API waits for
step 1's bitrate finding. Step 3 needs step 2's ryll pull request
merged. Steps 4, 5 and 6 run in order, after step 3. Step 7 needs step
6. Steps 8, 9 and 10 follow in order.

### Brief 0: commits and the ryll worktree

Commit the phase 7 close-out (master plan Execution table and
`index.md`), then this plan with the master plan and index corrections.
Create a ryll worktree from `origin/develop` on a branch named for this
phase (`andris-phase-08-streams`, matching phase 7's ryll branch
naming) for step 2.

### Brief 1: spike

Gated: report results to the management session before step 2's
encoder API is fixed. Use a throwaway crate under the scratchpad, built
in the `andris-dev` image with nasm installed in a derived image. Do not
touch `:10`. Answer, with numbers:

1. Encode cost per frame, including BGRA to I420, at 320×240, 640×360,
   1280×720 and 1920×1080, screen-content usage, with and without
   nasm, single-threaded and with `num_threads(0)`.
2. Whether the openh264 crate can change the target bitrate of a live
   encoder. Look for a `set_bitrate`-like method, or reach the raw
   `ISVCEncoder::SetOption(ENCODER_OPTION_BITRATE, …)` through the
   crate's raw API. If it can, measure how quickly the output size
   follows. If not, measure the cost of re-creating the encoder plus
   an IDR.
3. Whether `openh264::encoder::Encoder` is `Send`, so it can move into
   `spawn_blocking` and back. If not, say what a dedicated encoder
   thread per stream would need.
4. That ryll's `shakenfist-spice-compression` decoder
   (`video::for_stream`, codec 3) decodes the probe's output.
5. A python-xlib script, run under Xvfb in the `andris-dev` image,
   that paints a 320×240 moving photographic pattern at about 25 fps
   with `put_image`. Report its CPU use and the damage rate Xvfb
   reports for it.

### Brief 2: ryll

In a ryll worktree, following ryll's `AGENTS.md`, one pull request with
one commit per item:

1. **Move `H264Encoder`** from `shakenfist-spice-renderer/src/encoder/h264.rs`
   into `shakenfist-spice-compression` behind its `encode` feature.
   - Put it beside `encode_spice_lz4` and `encode_spice_jpeg`, taking
     the same `Bgrx` input.
   - The renderer re-exports it, so `encoder/task.rs` and
     `ryll/src/capture.rs` are unchanged.
   - Add what andris needs: change the target bitrate (in place or by
     re-creation, per step 1), a frame-rate cap, and a "skipped"
     result that carries no bytes.
   - The existing tests move with it, and a round trip through
     `video::for_stream` covers it.
2. **Advertise H.264 by default.**
   - `link::display_channel_caps` always includes `CODEC_H264`.
   - `PREFERRED_VIDEO_CODEC_TYPE` sends `[MJPEG, H264]` by default, and
     `[H264, MJPEG]` with `--experimental-h264`, whose help and
     `docs/configuration.md` entry change to say "prefer H.264".
   - Add `--no-h264`, which drops the cap, for e2e's no-stream check
     and as an escape hatch.
   - Explain in `docs/spice-protocol.md` why spice-server still
     chooses MJPEG (its codec order follows the client's).
3. **Fix stream compositing for a frame wider than its dest**
   (`channels/display.rs:1873-1949`, `display/surface.rs:78`).
   - Crop the frame to the dest rect, honouring its stride, before
     emitting `ImageReady`.
   - Test with a frame wider and taller than the dest.
4. **Control protocol 1.4: a `stats` verb** (ryll#512, which it
   closes).
   - It returns the display channel's `DisplaySnapshot` stream
     counters: `streams_active`, `streams_created_total`,
     `streams_destroyed_total`, `stream_data_orphan_count`,
     `stream_reports_sent_total`, and per stream its codec, size,
     `frames_decoded_ok` and decode failures.
   - Document it in `docs/control-socket-protocol.md`.
   - Correct `docs/multi-mode-parity.md:90-93`, which says headless
     frames are counted, not rendered.

Push and open the pull request when the operator asks.

### Brief 3: bump the ryll pin, add nasm

- Bump all three ryll crates in `andris/Cargo.toml` to step 2's merge
  commit, and enable `encode`'s H.264 encoder as step 2 shapes it.
- Refresh `Cargo.lock` with `make lock`, and update the comment at
  `andris/Cargo.toml:41-42`.
- Add `nasm` to `.devcontainer/Dockerfile`'s package list, with a
  comment naming openh264's assembly and step 1's speed-up.
- Check that `make build test lint` passes, and that a release build's
  openh264 compiled its assembly: the build script prints no "Failed
  to compile NASM files".

### Brief 4: the stream lifecycle

In `andris/src/server/`, without detection yet:

1. **Opcodes.** Add `StreamCreate`, `StreamData`, `StreamDestroy`,
   `StreamDestroyAll` and `StreamActivateReport` to `DisplayOpcode`
   (`opcodes.rs`). The `opcodes_are_named_by_ryll` test must cover
   them.
2. **Caps.** Read the client's `MULTI_CODEC`, `CODEC_H264` and
   `STREAM_REPORT` display caps into the display channel's state
   (`display.rs`, beside the LZ4 cap at :367-371).
3. **A `stream` module** (`server/updates/stream.rs`, or
   `server/stream.rs` if it reads better) owns one stream:
   - its id, cycling through 0..63, with one live at a time;
   - its rectangle;
   - the encoder;
   - the report state (`unique_id`, last report).

   It builds `STREAM_CREATE` with flags `TOP_DOWN`, codec 3, stamp 0,
   stream, src and dest all the rectangle's size, and clip none.

   It builds `STREAM_DATA` with `multi_media_time` per decision 6. It
   sends `STREAM_ACTIVATE_REPORT` after `STREAM_CREATE` when the
   client has `STREAM_REPORT`.

   The encoder runs off the async thread (step 1's finding decides
   how), and a skipped frame sends nothing.
4. **Reports.** Parse `STREAM_REPORT` into the stream's state instead
   of ignoring it (`display.rs:341-349`), and ignore one for another
   stream or `unique_id`. The "cannot decode" report ends streams for
   the connection, per decision 5.
5. **Surface replacement** sends `STREAM_DESTROY_ALL` before
   `SURFACE_DESTROY`.
6. **Tests.** In `tests/display.rs`, with a test-only hook that starts
   a stream over a given rectangle:
   - create, data and destroy reach a client that has `CODEC_H264`;
   - the frames decode with ryll's `video::for_stream` (dev-dependency
     features) to pixels within a tolerance of the synthetic desktop;
   - no stream starts without the cap;
   - a foreign report is ignored;
   - a "cannot decode" report ends streams with one warning;
   - a resize sends `DESTROY_ALL` first.

### Brief 5: detection, the frame path and teardown

1. **Heat tracker** (`server/updates/heat.rs`). For each tile it
   keeps the recent frames that changed it and whether those draws
   were JPEG. It finds candidates per decision 3. Pure, with unit
   tests:
   - a moving photographic area streams after 20 frames;
   - a scrolling LZ4-only area never streams;
   - a 1×1-tile animation never streams;
   - a 250 ms pause resets the count.
2. **The frame path** (`Display::frame()`, `display.rs:458-493`).
   - While a stream lives, its rectangle's tiles are taken out of the
     region before draws are encoded.
   - If any were dirty, the rectangle is captured and sent as one
     stream frame.
   - The shadow and `LossyTiles` are updated per decision 5.
   - The rectangle is tile-aligned, so `Region` subtraction is exact.
     A pixel row or column outside the even-rounded rectangle is
     drawn as usual.
3. **Teardown and replacement** per decisions 4 and 5: quiet for 1 s,
   outgrown, encoder failure, or a resize. After teardown the lossy
   tiles are due at once.
4. **The synthetic platform** gains an `animate(rect)` helper that
   repaints a rectangle with a photographic pattern that differs every
   call, without growing the overlay list (`synthetic.rs:31-35`).
5. **Tests in `tests/display.rs`:**
   - animating a 256×192 area at the test frame interval starts one
     stream, and frames flow;
   - draws keep flowing for a separate change outside the stream;
   - stopping the animation destroys the stream within 1.5 s, and the
     client's screen then matches the desktop exactly;
   - continuous `advance()` (LZ4-only) never streams.

### Brief 6: rate control and metrics

1. Implement decision 7 in the stream module. The adjustment is a pure
   function from (current target, report or server drop, time since
   last change) to a new target, unit tested for each rule and its
   floor and cap.
2. Wire server drops: a stream frame skipped because `may_send()` is
   false counts as one.
3. Add the stream fields of decision 9 to `stats.rs` and the metrics
   line. Extend `ENCODINGS` and `encoding_slot` deliberately.
4. Tests:
   - a client sending reports with drops sees the target fall, through
     a test hook that reads the target;
   - clean reports raise it after 2 s;
   - the metrics line carries the new fields.

### Brief 7: end-to-end checks

1. **`scripts/e2e-video.py`**: a python-xlib window painting a moving
   photographic 320×240 pattern at about 25 fps for a set time, beside
   a static text area.
2. **In `scripts/e2e.sh` and `e2e-drive.py`**, driving ryll through
   protocol 1.4:
   - with the script running, `stats` shows one H.264 stream created,
     frames decoded and no decode failures;
   - within 2 s of the script stopping, the stream is destroyed;
   - a screenshot of the area then matches the script's last frame
     exactly;
   - the static text area was never streamed. Check from andris's log
     that the stream rectangle does not cover it.
3. **A second ryll with `--no-h264`** sees no stream, and the area
   arrives as JPEG draws (andris's metrics line).
4. **`make e2e-xrdp`: X12.** The same video script on `:50` under
   xorgxrdp, with FreeRDP connected, streams to ryll. Check RDP is
   unaffected (the FreeRDP process stays up, with no errors in its
   log).

### Brief 8: documentation

- **`docs/running.md`:**
  - a "Video streams" section: what streams, when it starts and stops,
    the lossless repair, the H.264 caps a client needs, ryll's
    defaults and `--no-h264`;
  - the new metrics fields;
  - replace the "no video" lines (:275, :350, :416-418);
  - the patent position per decision 2.
- **`ARCHITECTURE.md`:** the stream path in "How a frame reaches the
  wire", and the stale "LZ4 or uncompressed" line (:144-145).
- **`README.md:22` and `docs/index.md:14`:** video exists; audio does
  not yet.
- **`docs/development.md`:** nasm in the image, and the e2e video
  check.
- **Master plan:** Future work gains several streams per channel,
  whole-screen H.264, and streams for clients without H.264. (Open
  question 7, the patent question, was added with this plan.)

### Brief 9: operator acceptance on `:10`

With the operator, on the real session, using the Mac's ryll at the
release carrying step 2:

- **Playback.** Play a video in a browser on the desktop at a normal
  window size. Ryll's status bar shows the stream glyph green.
  andris's log shows one stream created, and the metrics line shows
  stream frames.
- **No flapping.** The glyph never turns amber or red over two
  minutes of playback.
- **Pause.** Pause the video. Within about a second the stream ends,
  and the player's controls and any text near it are sharp.
- **Typing.** Typing in a terminal while the video plays elsewhere
  stays responsive, and the terminal is never streamed.

### Brief 10: push audit

Run `PUSH-AUDIT.md` over `origin/develop...HEAD` in two waves, as
phases 5 to 7 did. Fix what is blocking, record what is declined, and
open the pull request when the operator asks.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| H.264's 4:2:0 blurs text inside a stream | The photographic filter (decision 3), and the lossless repair at teardown (decision 5). Step 5 tests that a non-photographic area never streams, and step 9 checks text near a paused video by eye. |
| Streams flap on and off | Hysteresis: 20 frames to start, 1 s to stop, and a cluster that stays inside the rectangle keeps it. Step 9 watches ryll's flap glyph; the management session reads created/destroyed counts in the metrics line. |
| Encoding a large rectangle at 30 fps costs too much CPU | Step 1 measures it, and nasm brings 1080p to about 10 ms. Rate control's frame skipping sheds load. If step 1 shows more than half the frame budget at 1080p, the plan caps the stream area, and says so in the Result section. |
| Every frame reports as late, so rate control collapses | Decision 6's latency budget. Step 6's tests feed reports with and without lateness. The management session checks a real ryll's report fields in step 7's logs. |
| A ryll release without step 2 never sees streams | That is the intended fallback: without `CODEC_H264` andris sends JPEG draws as today. Step 7 checks the no-cap path. |
| spice-gtk (remote-viewer) adds up to 80 ms of latency | Accepted (decision 6); recorded in `docs/running.md`. remote-viewer is best effort per the master plan. |
| openh264 patent exposure in what andris ships | Documented in step 8; decided for the wheels in phase 10 (decision 2). |
| The spike finds bitrate cannot change in place | Re-create the encoder with an IDR, rate-limited to one change a second. Step 1 measures the cost before step 2 fixes the API. |

## Definition of done

- **The checks pass.** `make lint test e2e` passes, and `make e2e-xrdp`
  passes locally with X12 recorded in the Result section.
- **Ryll's half has merged.** The ryll pull request's merge commit is
  the pin in `andris/Cargo.toml`. `git -C ryll grep -n "CODEC_H264"
  origin/develop -- ryll/src/` shows it in the default caps. The
  master plan's `Merged` cell names both pull requests.
- **The encoder lives in the compression crate.** `grep -rn
  shakenfist_spice_renderer andris/` finds nothing.
- **nasm is used.** `.devcontainer/Dockerfile` installs it, and a
  release build's log has no "Failed to compile NASM files".
- **Streams are H.264 only.** `grep -n "codec_type" andris/src`
  shows only codec 3, and a test shows no stream without
  `CODEC_H264`.
- **Text is not streamed.** A test shows continuous LZ4-only change
  never starts a stream, and e2e shows the static text area outside
  the stream's rectangle.
- **The area is repaired.** e2e's screenshot after teardown matches
  exactly.
- **Rate control responds.** Unit tests cover each rule of decision 7,
  and an integration test shows a falling target under drops.
- **Documentation.** `grep -n -i "no video" README.md docs/index.md
  docs/running.md ARCHITECTURE.md` finds nothing.
  `docs/running.md` states the patent position. `ARCHITECTURE.md` no
  longer says images are only LZ4 or uncompressed.
- **Deferred work is tracked.** Future work lists several streams per
  channel, whole-screen H.264 and non-H.264 clients. Open question 7
  carries the patent question to phase 10, and every ryll gap found
  has an issue.

## Result

Filled in as the steps complete.

### Step 1: spike

Measured in the `andris-dev` image on a Ryzen 7 8845HS, with other
builds running (two runs agreed within about 5%). The content was
drifting gradients with ±12 per-channel noise changing each frame, at
ryll's bitrate formula for 30 fps. Times are median / p95 ms per frame,
including BGRA to I420:

| Size | nasm, 1 slice | nasm, 4 slices | no nasm, 1 slice | Bytes/frame |
|------|---------------|----------------|------------------|-------------|
| 320×240 | 0.66 / 0.79 | 0.33 / 0.56 | 1.90 / 2.49 | 3164 |
| 640×360 | 1.69 / 2.21 | 0.93 / 1.69 | 5.03 / 6.60 | 4165 |
| 1280×720 | 6.62 / 9.12 | 3.46 / 6.10 | 18.8 / 25.6 | 11524 |
| 1920×1080 | 15.8 / 21.1 | 7.20 / 12.5 | 42.8 / 57.4 | 25898 |

- **The frame budget.** Single-sliced 1080p with nasm exceeds half of
  a 33 ms frame at p95, which the risk table said would cap the stream
  area. Four slices (`SM_FIXEDSLCNUM_SLICE`, set through the raw
  `ENCODER_OPTION_SVC_ENCODE_PARAM_EXT`) bring it to 12.5 ms at p95,
  for 0–4% more bytes, so the encoder uses slices instead and the area
  is not capped. `num_threads(0)` alone does nothing, because the crate
  forces a single slice.
- **Conversion.** `YUVBuffer::read_bgra8` takes 0.8 ms at 1080p; ryll's
  per-pixel `from_rgb_source` takes 12.7 ms. The moved encoder uses the
  former.
- **Bitrate changes in place**, through `raw_api().set_option` with
  `ENCODER_OPTION_BITRATE`; output follows within 1–5 frames. A raise
  above the creation rate must first lift `ENCODER_OPTION_MAX_BITRATE`
  on `SPATIAL_LAYER_0` (on `SPATIAL_LAYER_ALL` it returns success and
  does nothing). Options apply only after the first encode. No
  re-creation is needed.
- **Two traps.** `ScreenContentRealTime` silently clamps the default
  QP range to 26–35, so targets above about 2.4 Mbit/s at 640×360 and
  low targets are both unreachable; an explicit `QpRange` with a
  minimum of 12 or more fixes it. Bitrate mode budgets each frame as
  target ÷ frame rate, ignoring timestamps, so rate control must keep
  `ENCODER_OPTION_FRAME_RATE` at the observed frame rate.
- **Other answers.** `Encoder` is `Send` and `Sync`. A skipped frame
  is `FrameType::Skip` with no bytes. Ryll's `video::for_stream(3, …)`
  decoded every frame of every case. The nasm build's script output
  has no "Failed to compile NASM files"; the no-nasm build's does,
  visible only under `cargo -vv`.
- **Picture source.** A python-xlib script holds 25.0 fps over
  320×240 for about 4% of a core, and Xvfb reports 25 repaints a
  second over the area, with none once frozen. It is kept for step 7.

## Back brief

Before executing any step of this plan, please back brief the operator
as to your understanding of the plan and how the work you intend to do
aligns with that plan.

### Step 2: ryll

Ryll's half merged as ryll#552 (`a157ced`), in four commits: the
stream crop fix, H.264 advertised after MJPEG with `--no-h264`, the
control socket's `stats` verb (protocol 1.4, closing ryll#512), and
the encoder's move into `shakenfist-spice-compression` with the API
the spike called for. Two choices departed from the brief:

- **Ryll's own output stays at one slice.** Its WebRTC encoder uses
  the moved encoder through a thin adapter, and browsers have not been
  tried with multi-slice frames. Andris gets four.
- **The spice-streaming-agent exception.** spice-server offers a
  streaming agent every codec the client has caps for, ignoring the
  client's order, so such a guest may now choose H.264 and hit
  ryll#398. The operator accepted this on 2026-10-16; `--no-h264` is
  the workaround (ryll#547).

The PR merged before its review round was pushed, so that round went
to ryll#554. Gaps found along the way are ryll#543 to ryll#551.
