# Phase 9: Audio

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: medium, as the master plan asks. Audio follows
patterns phases 7 and 8 set: a small ryll pull request to the shared
crates first, then a platform seam and a channel in andris. The
survey found two of the master plan's premises wrong (findings 1 and
2), and the decisions below rest on the corrected ones.

## Scope

In scope, as one ryll pull request followed by one andris pull
request:

- **Ryll first** (decision 1, step 2):
  - playback wire types and constants in
    `shakenfist-spice-protocol`, readable and writable, which ryll's
    own playback channel then parses with;
  - a `playback` section in the control socket's `stats`, so that
    e2e can assert on audio.
- **Capture** of the desktop's sound through a new `AudioSource`
  platform seam: the session sound server's default sink monitor,
  read over the PulseAudio protocol (decision 2), and a synthetic tone
  for tests.
- **The playback channel** in andris: `MODE`, `START`, `DATA` and
  `STOP`, Opus at 48 kHz in 10 ms frames, or raw PCM for a client
  without Opus (decision 3); starting and stopping with the sound
  (decision 4); multimedia time on each packet (decision 5); dropping
  rather than queueing when the client falls behind (decision 6).
- **Options** to turn audio off and to pick the monitor (decision 7).
- **The ryll pin moves to 0.3.x** (step 3), which brings ryll#559's
  `#[non_exhaustive]` `H264Config`.
- **Checks:** unit and integration tests on the synthetic tone,
  `make e2e` with a PulseAudio daemon and a tone in the devcontainer,
  and one `make e2e-xrdp` check if the spike finds an xrdp sink module
  for the test bed.
- **Documentation**, and operator acceptance on `:10` with a video
  playing.

Out of scope:

- **Forwarding the desktop's volume and mute** (decision 8). Andris
  advertises the `VOLUME` cap but sends neither message. This goes to
  Future work.
- **The record channel** (the microphone). Already in the master
  plan's "not covered" list and Future work.
- **Audio/video sync in ryll.** Ryll plays audio as it arrives and
  shows video as it arrives (finding 6). Andris stamps both on one
  clock, which is what spice-gtk syncs on; ryll's gap gets an issue.
- **Creating a sink, or changing the session's default sink.** Andris
  attaches to the desktop and changes nothing, as it does for X.
- **Installing an xrdp sink module on the host.** That is the
  operator's choice, documented in `docs/xrdp.md`.

## What the survey found

1. **The development host has no xrdp sink.** The master plan says
   andris captures "the xrdp sink on the development host".
   - The host runs PipeWire 1.4.2 with `pipewire-pulse` and
     WirePlumber, for the operator's user and for lightdm.
   - Neither `pipewire-module-xrdp` nor `pulseaudio-module-xrdp` is
     installed.
   - The host has two HD-Audio cards, so the session's default sink
     is most likely a local output, not a dummy.
   - This was established from `dpkg -l`, the process list and
     `/proc/asound/cards`, without connecting to the operator's
     sound server.
2. **Volume and mute go from server to client, not the other way.**
   The master plan says andris "honours the client's volume and mute
   messages".
   - The playback channel has no client-to-server messages beyond
     the common ack, ack-sync and pong (`spice.proto:1189-1237`;
     `enums.h:568-578`).
   - `VOLUME` (`u8 nchannels, u16 volume[]`) and `MUTE` (`u8`) carry
     the *desktop's* settings to the client. spice-server sends them
     only to a client with `PLAYBACK_CAP_VOLUME`
     (`server/sound.cpp:403`, `:426-450`). spice-gtk applies
     `volume[0]` to its local output (`spice-gstaudio.c:350-423`).
   - The client's own volume reaches a guest only through the
     vdagent's `VD_AGENT_AUDIO_VOLUME_SYNC`
     (`spice-gtk/src/channel-main.c:1203-1257`). Andris's in-process
     agent does not handle it, and phase 9 does not add it.
3. **Ryll's protocol crate has playback opcodes and names, but no
   message types and no caps.**
   - `playback_server` (DATA 101 to LATENCY 107) is in
     `shakenfist-spice-protocol/src/constants.rs:422-433`. The
     `logging::message_names` table names all seven
     (`logging.rs:522-536`), so andris may send any of them.
   - There are no `PLAYBACK_CAP_*`, audio data-mode or audio format
     constants. The renderer keeps `AUDIO_DATA_MODE_RAW` and `_OPUS`
     privately (`shakenfist-spice-renderer/src/channels/playback.rs:25-26`)
     and parses each message by hand (`playback.rs:1105-1271`).
   - Main's `MainInit.multi_media_time`, `MultiMediaTime` and
     `ChannelsList` already have full read and write
     (`messages/main.rs:17-192`).
4. **Kerbside allows the playback channel.** The allowlist maps
   playback in both directions to ryll's tables
   (`rust/kerbside-proxy/src/allowlist.rs:126-127`).
   - Kerbside answers the client with `VOLUME | OPUS`
     (`caps.rs:49-51`, `:83`), and forwards the client's own caps to
     the backend unchanged. It only warns when a backend lacks a cap
     it offered (`backend.rs:279-290`).
   - So andris sees ryll's caps, which are CELT and OPUS (bits 0 and
     3: `DEFAULT_OTHER`, `constants.rs:133-135`), not VOLUME.
5. **What spice-server does**, as behaviour to match, not code to
   translate:
   - **Order.** `MODE`, then `START`, then `VOLUME` and `MUTE` if
     set, then `DATA` (`sound.cpp:976-999`, `:623-671`). The mode is
     chosen once per connection: Opus when the client has
     `PLAYBACK_CAP_OPUS` and the rate is 48 kHz, otherwise raw S16LE
     stereo (`sound.cpp:921-974`).
   - **Frames.** 480 samples a channel (10 ms at 48 kHz) in both
     modes, so a raw payload is 1920 bytes. An Opus packet is at most
     480 bytes (`snd_codec.h:25-33`).
   - **spice-gtk decodes into a buffer of exactly 10 ms**
     (`channel-playback.c:314-325`), and spice-html5 assumes 10 ms.
     Longer Opus frames would fail there.
   - **Encoder.** `OPUS_APPLICATION_AUDIO`, stereo, with libopus's
     default bitrate and complexity (`snd_codec.c:71-118`).
   - **Start and stop come from the producer.** QEMU calls them when
     the guest's voice opens or closes. spice-server does no silence
     detection and no pacing of its own (`sound.cpp:830-878`).
   - **No acks, and drops over queueing.** The channel has no ack
     window, and one queued frame at most. A new frame replaces a
     queued one, by design: "prefer losing samples to queueing"
     (`sound.cpp:80-87`, `:911-916`).
6. **Multimedia time, and who uses it.**
   - spice-server stamps `START`, `DATA` and video frames from one
     clock with no offset. Since v0.16.0 it never sends
     `PLAYBACK_LATENCY` (spice commit 7e56fc9f).
   - **spice-gtk** pushes `DATA` straight into a live GStreamer queue
     and does not schedule by its time. Instead, every second it sets
     its session clock to the last `DATA` time minus the sink's
     latency. Video frames then wait or drop against that clock, so
     video follows the audible audio (`spice-gstaudio.c:259-347`;
     `channel-display.c:1790-1812`). Times must not go backwards
     (`channel-playback.c:307-310`).
   - **Ryll** ignores playback times entirely. It plays samples in
     arrival order through a ring of up to 2 s
     (`playback.rs:31`, `:1126`, `:1161-1166`). Its multimedia clock
     only feeds the display channel's stream reports. There is no
     A/V sync and no drift correction.
   - **Andris** sets `multi_media_time` in `MAIN_INIT` from the
     session's elapsed time (`andris/src/server/main_channel.rs:113`),
     and stamps stream frames 80 ms ahead of it (phase 8, decision 6).
7. **Ryll's headless build has no audio, and its control socket says
   nothing about audio.**
   - Playback is behind the default `audio` feature
     (`ryll/Cargo.toml`), which pulls in cpal and opus-decoder.
     Andris's `make ryll-headless` builds with
     `--no-default-features` (`docs/development.md`), so e2e's ryll
     never opens the playback channel.
   - Ryll built with `audio` and no sound device still decodes: its
     audio thread retries opening the device with a backoff, while
     DATA is counted and decoded into the ring (`playback.rs:72-90`).
   - The counters an e2e test wants exist in `PlaybackSnapshot`
     (`renderer/src/snapshots.rs:667-760`), but reach only bug
     reports. `stats` returns display counters only
     (`control/protocol.rs:335-337`, protocol 1.5).
8. **Ryll is about to release 0.3.0** (ryll#573). Nothing in it
   touches audio. Andris's `^0.2.0` requirements will not match it,
   and ryll#559 makes `H264Config` `#[non_exhaustive]`. Andris builds
   one with a struct expression over `..H264Config::new(..)`
   (`andris/src/server/updates/stream.rs:514-516`), which a
   non-exhaustive struct forbids outside its crate, so it must become
   `H264Config::new` plus field assignments. Step 3 does both.
9. **x11spice has no audio.** Nothing in its tree registers a
   playback interface (`x11spice/src/spice.c:650-660`), so there is
   no sibling design to compare with.
10. **The master plan's other audio claims hold.** Kerbside's
    playback caps are `VOLUME` and `OPUS` as the Situation says, and
    the success criterion "a video played on the desktop arrives as
    an H.264 stream with audio" stands.

Findings 1 and 2 are corrected at their source, in the master plan's
phase 9 section and its `index.md` row, by the commit that adds this
plan. Later steps need not redo that.

## Decisions

1. **Ryll first: wire types and a `stats` section, nothing more.**
   - `shakenfist-spice-protocol` gains `PLAYBACK_CAP_*` (CELT 0,
     VOLUME 1, LATENCY 2, OPUS 3), `AUDIO_DATA_MODE_*` (RAW 1, OPUS
     3), `AUDIO_FMT_S16` (1), and `WireType` types for `START`,
     `MODE`, `DATA`'s header, `VOLUME`, `MUTE` and `LATENCY`.
   - The renderer's playback channel parses with them, so the client
     and server roles are tested against each other, as phase 2 did
     for the other channels.
   - `stats` gains a `playback` object (protocol 1.6, additive). Its
     fields are starts, stops, packets received and decoded, decode
     failures, bytes, and the current codec, rate and channels.
   - Ryll's headless build stays audio-free by default. Andris's
     `make ryll-headless` adds `--features audio`, and the
     devcontainer gains `libasound2-dev` for cpal.
   - Ryll does not change which caps it advertises, and does not
     apply `VOLUME` or `MUTE`. Its missing A/V sync gets an issue, as
     does its unbounded drift (finding 6).
2. **Capture over the PulseAudio protocol, from the default sink's
   monitor.**
   - Andris reads `@DEFAULT_MONITOR@` through libpulse's simple API
     (the `libpulse-simple-binding` crate), on its own thread, as
     S16LE stereo at 48 kHz, asking for 10 ms fragments.
   - This works against PulseAudio, against PipeWire's
     `pipewire-pulse` (the host's case, finding 1), and against an
     xrdp sink module loaded into either. A native PipeWire client
     (`pipewire-rs`) would serve only PipeWire, and needs bindgen and
     clang in the image.
   - Capture runs only while a client's playback channel is open.
     Andris does not hold the sound server, or keep a sink awake,
     with nobody listening.
   - The monitor is resolved when capture opens. If the default sink
     changes during a session, andris keeps the old one until the
     playback channel reconnects. This is documented, not chased.
   - The cost is a runtime dependency on `libpulse0`, which phase 10's
     packaging must declare.
   - The seam is an `AudioSource` trait in `andris/src/platform/`,
     beside the others. A synthetic source makes a tone, for tests
     and for `--platform synthetic`.
3. **Opus at 48 kHz in 10 ms frames, or raw PCM.**
   - When the client has `PLAYBACK_CAP_OPUS`, `MODE` says Opus.
     Otherwise it says raw, and `DATA` carries 1920-byte S16LE
     frames. This follows spice-server (finding 5).
   - The encoder is the `opus` crate (0.4, libopus built from bundled
     source with cmake, which the image has), as ryll's WebRTC
     bridge uses. It runs stereo, `Application::Audio`, at an
     explicit 96 kbit/s. Libopus's default for 10 ms stereo frames is
     about the same, and naming it makes it a metric andris can
     report.
   - **Frames are always 10 ms (480 samples).** spice-gtk cannot
     decode longer ones (finding 5).
   - Andris advertises `VOLUME | OPUS`, the caps kerbside offers on
     andris's behalf, so kerbside has nothing to warn about.
4. **Start and stop with the sound, judged by digital silence.**
   - A monitor delivers silence when nothing plays. Andris sends
     `STOP` after 1 s in which every sample is within ±8 of zero. It
     sends `START`, then that frame, when a frame next exceeds the
     threshold.
   - This saves about 100 kbit/s and the encoder's CPU on an idle
     desktop. It also gives spice-gtk a real stop, which returns its
     clock to the main channel's.
   - spice-server leaves this to QEMU, which knows when a voice
     opens. Andris sees only samples, so silence is the only signal
     it has. The threshold and hold are constants, with the
     reasoning beside them.
   - **This is the decision most open to argument.** Streaming
     continuously is simpler, and Opus encodes silence cheaply. The
     plan stops anyway because an idle desktop should cost the
     client nothing, as the display channel's damage-driven design
     already promises.
5. **One clock for audio and video.**
   - `START`'s time, and each `DATA`'s, is the session's multimedia
     time at the moment the frame was captured: the elapsed time when
     andris read it, less the capture latency libpulse reports. Times
     never go backwards (finding 6).
   - Video keeps phase 8's stamps, 80 ms ahead on the same clock. So
     spice-gtk, which sets its clock from audio, delays video to
     match what it plays. Ryll, which syncs nothing, plays each as it
     arrives.
   - Andris sends no `PLAYBACK_LATENCY`, as spice-server sends none.
6. **Drop, never queue.**
   - Capture hands frames to the channel's task through a channel of
     three frames (30 ms). When it is full, the oldest frame is
     dropped and counted. This follows spice-server's "prefer losing
     samples to queueing".
   - The playback channel has no ack window, and it sets
     `TCP_NODELAY` like andris's other channels.
   - Pacing comes from the sound server's clock. Andris sends each
     frame as capture delivers it, and never on a timer of its own.
7. **Options: `--no-audio` and `--audio-source`.**
   - `--no-audio` leaves the playback channel out of
     `CHANNELS_LIST`, and refuses a playback link.
   - `--audio-source NAME` names a PulseAudio source, with a default
     of `@DEFAULT_MONITOR@`.
   - When capture cannot open (no sound server, or the source is
     gone), andris logs it once. The playback channel stays linked
     and silent, and capture is retried each time the channel links.
     Audio failing never ends the session.
   - Playback is listed before inputs in `CHANNELS_LIST` (ryll#509,
     now fixed, broke channels listed after inputs).
8. **No `VOLUME` or `MUTE` in this phase.**
   - Ryll does not advertise the cap, so only remote-viewer would see
     them. Forwarding them needs libpulse's asynchronous API, to
     follow the sink's volume.
   - Whether a monitor carries the signal before or after the sink's
     volume and mute is the spike's question (step 1). If it is after
     them, forwarding them would attenuate twice.
   - Instead the docs say what the operator gets. If muting the local
     speakers also silences the monitor, that matters to someone
     working remotely on a machine with speakers, and the docs say so.
9. **Metrics.** The metrics line gains a playback block: audio
   frames sent, kbit/s, Opus encode ms, frames dropped, starts and
   stops. Its session-end line carries the totals.

## Execution

Implementation is done by sub-agents briefed from this plan. The
management session reviews each diff and makes each commit. For the
ryll step, the rules are ryll's: its `make lint` and `make test`, and
its conventions in its `AGENTS.md`.

- Every andris commit must leave `make lint` and `make test` passing.
- Cargo runs only through the `Makefile`, never on the host.
- No code is translated from spice-server, spice-gtk, x11spice, QEMU,
  xrdp or its sink modules. Their behaviour is cited from the survey;
  local copies are in `/srv/src-reference/spice/`.
- Every step adds tests for what it adds.
- **No sub-agent connects to, signals, captures or injects into the
  host's `:10` session**, nor touches its xrdp, sesman or chansrv. The
  host shell has `DISPLAY=:10.0`: unset `DISPLAY` and `XAUTHORITY`
  for every X command and make target. All xrdp testing happens in the
  test-bed container, on `:50`.
- **No sub-agent connects to the host's sound server** (the
  operator's PipeWire). All audio testing runs inside a container
  with a sound server of its own. Unset `PULSE_SERVER` and
  `PULSE_COOKIE`, and never mount `$XDG_RUNTIME_DIR` into a container.
  Only step 8, with the operator, captures on the host.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | In progress | Commit the phase 8 close-out and this plan; create the ryll worktree. See brief 0. |
| 1 | medium | opus | none | Not started | Spike: libpulse capture from a monitor, in containers under PulseAudio and PipeWire; Opus at 10 ms; ryll headless with audio; an xrdp sink module for the test bed. Gated. See brief 1. |
| 2 | medium | opus | none | Not started | Ryll: playback wire types and constants, the renderer parsing with them, and `stats`' `playback` section. Ryll PR. See brief 2. |
| 3 | low | sonnet | none | Not started | Andris: bump the ryll pin to step 2's merge (0.3.x), `H264Config::new`, and the image's audio packages. See brief 3. |
| 4 | high | opus | none | Not started | The `AudioSource` seam: libpulse capture, the synthetic tone, and the options. See brief 4. |
| 5 | high | opus | none | Not started | The playback channel: link and caps, `MODE`/`START`/`DATA`/`STOP`, Opus, silence, clock, drops and metrics. See brief 5. |
| 6 | medium | opus | none | Not started | End-to-end checks: `make e2e` with a sound server and a tone, `--no-audio`, and X13 if the spike allows. See brief 6. |
| 7 | medium | sonnet | none | Not started | Documentation. See brief 7. |
| 8 | medium | management | none | Not started | Operator acceptance on `:10`, with a video. See brief 8. |
| 9 | medium | management | none | Not started | Push audit, pull request, queue. See brief 9. |

Step 2 may start once step 1's Opus and libpulse answers are in.
Step 3 needs step 2's ryll pull request merged. Steps 4 and 5 run in
order after step 3. Step 6 needs step 5. Steps 7, 8 and 9 follow in
order.

### Brief 0: commits and the ryll worktree

Commit the phase 8 close-out (the master plan's Execution row and the
`index.md` row), then this plan with the master plan's corrections.
Create the ryll worktree for step 2 from ryll's `develop`, on a branch
named `playback-wire-types`.

### Brief 1: spike

Answer these in throwaway containers built from `andris-dev`, never
on the host's sound server. Write each answer, with numbers, into the
Result section, and stop for the operator if any answer changes a
decision.

- **libpulse capture.** In a container, run a PulseAudio daemon as an
  unprivileged user with a null sink as the default. With
  `libpulse-simple-binding`, record `@DEFAULT_MONITOR@` as S16LE
  stereo at 48 kHz, asking for a 1920-byte fragment. Measure:
  - the chunk sizes a read returns;
  - the latency `pa_simple_get_latency` reports;
  - what an idle sink's monitor delivers: exact zeros, or noise?
  - whether the monitor follows the sink's volume (set it to 50%) and
    mute.

  Then repeat it under PipeWire with `pipewire-pulse` and WirePlumber,
  the host's stack, since the two may differ on the last point.
- **Opus.** Check that the `opus` crate 0.4 builds in the image with
  nothing more than cmake. Encode 10 ms stereo frames at 96 kbit/s,
  and record the time per frame and the packet sizes. Then check that
  ryll's decoder, `opus-decoder` 0.1, decodes every packet.
- **Ryll headless with audio.** Build ryll `--no-default-features
  --features audio` in the image with `libasound2-dev`. Run it with no
  sound device against a SPICE server sending audio, and check that it
  decodes without spinning a core.
- **An xrdp sink for the test bed.** Is `pulseaudio-module-xrdp` or
  `pipewire-module-xrdp` packaged for the test bed's Debian? Does
  xrdp's session then have an xrdp sink as its default, with a
  monitor andris can read while FreeRDP is connected? If not, X13 is
  dropped, and the Result section says why.

### Brief 2: ryll

In the ryll worktree, following ryll's `AGENTS.md`:

- **Constants and types.** Add the constants and `WireType` types of
  decision 1 to `shakenfist-spice-protocol`, in a new
  `messages/playback.rs`. Use the layouts in survey finding 5 and
  `spice.proto:1189-1237`. Include round-trip tests, and tests on
  short and over-long bodies.
- **The renderer.** Switch `channels/playback.rs` to parse with the
  new types, deleting its private constants and hand parsing.
  Behaviour must not change: its existing tests must pass untouched.
- **`stats`.** Add the `playback` object to `stats` (protocol 1.6),
  fed from `PlaybackSnapshot`, and document it in
  `docs/control-socket-protocol.md`. It is absent, not zeroed, in a
  build without `audio`.
- **Issues.** File ryll issues for: no A/V sync (video ignores the
  audio clock), and no bound on the playback ring's drift. Cite the
  survey's references.
- Open the ryll pull request when the operator asks.

### Brief 3: bump the pin, add audio packages

- Pin ryll at step 2's merge commit, with version requirements of
  "0.3.0" or whatever ryll's crates then carry.
- Replace the `H264Config` struct expression in
  `andris/src/server/updates/stream.rs` with `H264Config::new` and
  field assignments (ryll#559).
- `.devcontainer/Dockerfile` gains `libpulse-dev` (for capture),
  `libasound2-dev` (for ryll's cpal), and `pulseaudio` and
  `pulseaudio-utils` (a daemon and `pacat`, for tests). Give each its
  reason in the Dockerfile's comment style.
- `make ryll-headless` adds `--features audio`.

### Brief 4: the `AudioSource` seam

- **The trait.** Add `AudioSource` to `andris/src/platform/mod.rs`.
  Opening it starts capture and returns a receiver of 10 ms frames,
  each S16LE stereo at 48 kHz with its capture time. Dropping the
  receiver stops capture. Add an `audio` field to `Platform`, set to
  `None` under `--no-audio`.
- **PulseAudio capture.** Implement it in
  `andris/src/platform/pulse.rs`, using `libpulse-simple-binding` on a
  dedicated thread (decision 2):
  - Re-chunk reads into exact 480-sample frames.
  - Stamp each frame with the session clock, less the reported
    latency.
  - Turn a failure into one warning and a closed receiver, never a
    panic.
- **The synthetic source.** Add a tone to `platform/synthetic.rs`: a
  sine with on and off periods, paced by a timer, so tests can
  exercise starting and stopping.
- **Options.** Add `--no-audio` and `--audio-source` to
  `andris/src/config.rs` (decision 7), with tests.
- **Tests.** Test the re-chunking and stamping as units. Gate a
  PulseAudio test behind an environment variable that step 6's e2e
  sets, since `make test` has no sound server.

### Brief 5: the playback channel

- **Linking.** Add `ServedChannel::Playback` to
  `andris/src/server/link.rs`, with reply caps `VOLUME | OPUS`. Add an
  opcode table in `opcodes.rs`, bounded by
  `message_names::playback_server`. Add the channel to
  `CHANNELS_LIST`, before inputs, unless `--no-audio`.
- **The handler.** Add `andris/src/server/playback.rs`:
  - choose the mode from the client's caps, and send `MODE` before
    anything else;
  - open capture when the channel links;
  - start and stop on silence (decision 4);
  - encode Opus, or pass raw PCM through (decision 3);
  - stamp times (decision 5), and drop on a full channel
    (decision 6).
- **Opus.** Run the encoder on `spawn_blocking`, or inline if step
  1's figures show it well under a millisecond a frame. Record which
  in the Result section.
- **Metrics.** Add decision 9's block to `stats.rs`.
- **Tests.** Write integration tests in `andris/tests/playback.rs` on
  the synthetic tone, decoding with ryll's new types and opus-decoder
  (as a dev-dependency). Cover:
  - the message order;
  - Opus with the cap, raw without;
  - `STOP` after silence, and `START` again on sound;
  - non-decreasing times;
  - drops under a stalled reader;
  - `--no-audio` leaving the channel out;
  - a capture failure keeping the session up.

### Brief 6: end-to-end checks

- **`make e2e`.** In `scripts/e2e.sh`, start a PulseAudio daemon in
  the container with a null sink as default, and point andris at it.
  `scripts/e2e-drive.py` plays a 440 Hz tone for five seconds with
  `pacat` and checks the following through `stats`' `playback`
  section:
  - one start;
  - Opus at 48 kHz, two channels;
  - at least 400 packets decoded and none failed;
  - a stop within two seconds of the tone ending.
- **`--no-audio`.** A second andris, run with `--no-audio`, must give
  ryll no playback channel.
- **X13 in `make e2e-xrdp`**, if step 1 found a module. With the xrdp
  sink as the session's default and FreeRDP connected, a tone played
  in the session reaches ryll, and FreeRDP stays up. Otherwise the
  Result section records why there is no X13.

### Brief 7: documentation

- **`docs/running.md`** gains an Audio section. Cover:
  - what is captured, the options and the codecs;
  - the silence rule;
  - the latency, and A/V sync in ryll compared with spice-gtk;
  - what muting the local speakers does to the remote stream (step
    1's answer);
  - the `libpulse0` runtime dependency.

  Remove "No audio" from its limitations.
- **`docs/xrdp.md`** says how to give RDP clients sound as well:
  install the xrdp sink module for the session's sound server. Andris
  then captures that sink's monitor.
- **`ARCHITECTURE.md`** gains the `AudioSource` seam and the playback
  channel in its component inventory and data flow.
- **`README.md`'s Status, and `docs/index.md`**, stop saying there is
  no audio.
- **`docs/development.md`** describes e2e's sound server and tone, and
  ryll's audio build.
- **The master plan's Future work** gains: forwarding the desktop's
  volume and mute; following a change of default sink mid-session;
  and the vdagent's `AUDIO_VOLUME_SYNC`.

### Brief 8: operator acceptance on `:10`

With the operator, on the real session, using the Mac's ryll at a
release carrying step 2, or a develop build:

- **Sound.** Play a video with sound in a browser on the desktop. The
  sound plays on the Mac. andris's log shows the playback channel
  linked with Opus, and the metrics line shows audio frames.
- **Steady.** Ten minutes of playback with no gaps or clicks the
  operator can hear. The metrics line shows no dropped frames.
- **Sync.** Lip sync looks acceptable in ryll. Ryll has no A/V sync,
  so record how far apart sound and picture seem, rather than gating
  on it.
- **Pause.** Pausing stops the audio frames within about a second
  (`STOP`), and playing again starts them.
- **Local mute.** Mute the host's own output, and record whether the
  sound still reaches the Mac.

### Brief 9: push audit

Run `PUSH-AUDIT.md` over `origin/develop...HEAD` in two waves, as
phases 5 to 8 did. Fix what is blocking, record what is declined, and
open the pull request when the operator asks.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| The monitor follows the sink's mute, so muting the host's speakers silences the remote sound | Step 1 measures it under PulseAudio and PipeWire; step 8 checks it on the host; step 7 documents it. Creating a sink of andris's own is out of scope, and goes to Future work if this bites. |
| Silence detection clips quiet starts or flaps on quiet passages | A 1 s hold before `STOP`, and the frame that breaks silence is sent rather than lost. Step 5 tests both; step 8 listens. |
| Ryll's ring drifts, so latency grows over a long session | Pacing follows the sound server's clock (decision 6), and the ring's drift is filed against ryll (step 2). Step 8 watches ten minutes. |
| Capture latency is misreported, so spice-gtk's sync is off | The stamp subtracts what libpulse reports, and step 1 measures it. remote-viewer is best effort per the master plan. |
| A sound server in the e2e container flakes | Step 1 proves the daemon runs unprivileged in the image first. Step 6's checks have margins of about 2x, and read until a condition holds, as phase 8's do. |
| Linking libpulse makes andris fail to start where it is missing | The dependency is documented (step 7), and phase 10's packaging declares it. Andris without a sound server still runs (decision 7). |
| A sub-agent reaches the operator's PipeWire | The Execution rules forbid it, and containers do not mount `$XDG_RUNTIME_DIR`. The management session checks each step's commands. |

## Definition of done

- **The checks pass.** `make lint test e2e` passes. `make e2e-xrdp`
  passes locally, with X13 recorded in the Result section, or its
  absence explained there.
- **Ryll's half has merged.** The ryll pull request's merge commit is
  the pin in `andris/Cargo.toml`. `git -C ryll grep -n
  PLAYBACK_CAP_OPUS origin/develop -- shakenfist-spice-protocol/`
  finds the constant. The master plan's `Merged` cell names both pull
  requests.
- **Only 10 ms frames.** A test decodes every packet of a session
  with a 480-sample buffer.
- **Opus and raw both work.** Tests show Opus with the cap, and
  1920-byte raw frames without it.
- **Silence stops the stream.** A test shows `STOP` within 1.5 s of
  silence, and e2e shows a stop within 2 s of the tone ending.
- **Audio never ends a session.** A test shows a capture failure
  leaving the other channels up.
- **No sub-agent touched the host's sound server.** No step's commands
  set `PULSE_SERVER` to, or mount, the host's runtime directory.
- **Documentation.** `grep -n -i "no audio" README.md docs/index.md
  docs/running.md ARCHITECTURE.md` finds nothing. `docs/running.md`
  states what local mute does, with step 1's answer.
- **Deferred work is tracked.** Future work lists volume and mute
  forwarding, following a new default sink, and
  `AUDIO_VOLUME_SYNC`. Ryll has the two issues from step 2.

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.

## Result

Filled in as the steps complete.
