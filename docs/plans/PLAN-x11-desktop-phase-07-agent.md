# Phase 7: In-process agent: resize and clipboard

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: high, as the master plan asks. This phase turns on the
vdagent protocol's flow control, the X selection protocol, and how two
clipboard bridges behave in one session. The survey also found that the
phase's headline feature cannot work on the operator's xrdp session as
the master plan imagined it (survey finding 1), so the plan was
reshaped with the operator before it was written (decisions 1 and 2).

## Scope

In scope, as one ryll pull request followed by one andris pull request:

- **Ryll first: a headless clipboard and resize** (decision 2). Ryll's
  headless mode gains an in-memory clipboard, and its control socket
  gains verbs to set and read that clipboard and to ask for a resize,
  so that `make e2e` and `make e2e-xrdp` can drive the agent through
  real ryll.
- **The agent protocol on andris's main channel.** Andris announces an
  agent in `MAIN_INIT`, runs the token flow control in both directions,
  reassembles and chunks agent messages, announces capabilities, and
  dispatches monitors config and clipboard messages.
- **Two platform seams**, with X11 and synthetic implementations:
  display control (resize) and the clipboard.
- **Resize through RandR on an X server that allows it** (decision 1).
  On xorgxrdp, which refuses, andris notices the refusal, says so once,
  tells the client, and leaves the desktop at RDP's size.
- **The CLIPBOARD selection, as UTF-8 text, both ways**, coexisting with
  `xrdp-chansrv` (decisions 4 and 5).
- **Checks:** `make e2e` against Xvfb, and new interleaving checks in
  `make e2e-xrdp` with chansrv and FreeRDP's clipboard in the loop.
- **Documentation**, and operator acceptance on `:10` from the Mac,
  with the RDP client running on the same Mac.

Out of scope:

- **The PRIMARY selection** (decision 4). Ryll ignores it, macOS has
  none, and chansrv does not bridge it. It goes to Future work.
- **Images and files on the clipboard** (decision 4). Ryll only carries
  text. Images go to Future work, with the note that chansrv only takes
  `image/bmp`.
- **Changing xorgxrdp.** Resizing the xrdp desktop from SPICE needs an
  xorgxrdp change. The plan drafts an upstream feature request for the
  operator to file (step 8), and goes no further.
- **Physical pixels on a Retina Mac** (open question 2). Ryll asks for
  its window size in egui points, about half the pixel size on Retina.
  That is ryll's to fix, under ryll#485, and it does not matter on xrdp,
  where no resize applies.
- **Multi-monitor.** A monitors config with more than one monitor is
  applied as its first monitor.

## What the survey found

Three read-only surveys ran on 2026-10-14: andris at `498a72c`, ryll at
the pin `502199e` and at `origin/develop` (`86464db`), kerbside at
`origin/develop`, and the sources of xrdp v0.10.1, xorgxrdp v0.10.2,
spice-vdagent 0.23.0 and spice-server, read for behaviour only. Paths
into xrdp and xorgxrdp are relative to those tags' trees.

1. **xorgxrdp refuses every resize an X client asks for**, whether or
   not RDP is connected. Its screen-size hook returns FALSE unless an
   internal flag is set (`xorgxrdp module/rdpRandR.c:130-139`), and the
   flag is set only around xorgxrdp's own resizes, which come from xrdp
   when the RDP client resizes (`module/rdpClientCon.c:867-879`). The X
   server turns the refusal into `BadMatch` for `RRSetScreenSize`. The
   CRTC hook is a no-op that reports success (`rdpRandR.c:214-220`).
   There is no path from xorgxrdp back to xrdp for a size change made
   on the X side. The single output is named `rdp0`. Open question 1
   assumed the question was whether sharing RandR was *safe*; it is not
   possible at all. Step 1 confirms this live on the test bed before
   anything relies on it.
2. **Andris has no agent today.** `MAIN_INIT` says `agent_connected: 0`
   and `agent_tokens: 0` (`andris/src/server/main_channel.rs:103-104`),
   and `AGENT_START`, `AGENT_DATA` and `AGENT_TOKEN` fall through to
   `conn.ignore` (`:160`). `MainOpcode` has only `Ping`, `Init`,
   `ChannelsList` and `MouseMode` (`server/opcodes.rs:89-96`). Phase 4
   deferred validating agent token counts to this phase.
   `tests/main_channel.rs:61-62` asserts the agent is absent, and `:150`
   that `AGENT_START` is ignored; both change.
3. **Ryll's protocol crate models every message this phase needs**, at
   the pin, in `messages/vd_agent.rs` and `messages/main.rs`:
   `VdAgentMessageHeader`, `MonitorsConfig` (with the optional
   physical-size table), `AnnounceCapabilities`, `VdAgentReply`, the
   four clipboard messages (with the selection header when
   `VD_AGENT_CAP_CLIPBOARD_SELECTION` is negotiated), `AgentTokens`
   (one type for `AGENT_START`, `AGENT_TOKEN` both ways and
   `AGENT_CONNECTED_TOKENS`) and `AgentDisconnected`. `ClipboardGrab`
   does not model the grab serial (`vd_agent.rs:376-379`), so andris
   must not announce `VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL`, as the
   master plan said. Ryll's `message_names` names every agent opcode
   in both directions, and display `MONITORS_CONFIG`, so andris sends
   nothing ryll does not model.
4. **Agent reassembly is fixed in ryll, but after the pin.** ryll#474
   closed on 2026-10-11 with `messages/agent_stream.rs` and its
   `AgentReassembler`, capped at 100 MiB plus 4 KiB (ryll `de2e5a0`,
   `e17f299`, PR #495). The pin `502199e` predates it, so andris needs
   a pin bump (step 3), which brings other protocol-crate churn with
   it. Ryll at the pin also treats every `AGENT_DATA` as a new message,
   so a ryll older than #495 cannot read an agent message from andris
   that spans chunks. `docs/running.md` already says andris needs ryll
   from `develop`.
5. **Ryll's agent behaviour, which andris must meet:**
   - it sends `AGENT_START` with `u32::MAX` tokens and never sends a
     client `AGENT_TOKEN`;
   - it sends one chunk of at most 2042 bytes of payload per token the
     server has granted, so andris must hand tokens back as it
     consumes chunks, or ryll stalls;
   - it announces `MOUSE_STATE`, `MONITORS_CONFIG`, `REPLY`,
     `CLIPBOARD_BY_DEMAND` and `CLIPBOARD_SELECTION`, and its clipboard
     stays off until the agent announces its own capabilities;
   - its clipboard is CLIPBOARD only, `UTF8_TEXT` only, polled from the
     host every 500 ms; a grab offering UTF-8 makes it request at once;
   - it sends `MONITORS_CONFIG` 200 ms after the window settles, sized
     in egui points rounded down to a multiple of 8, one monitor, no
     physical sizes. On a new surface it auto-fits the window and
     records the size so as not to echo it back. The survey suspects
     that a resize made before the agent connects is never retried;
     step 2 checks.
6. **Ryll's headless mode cannot exercise either feature.** Its resize
   sender is created and never used (`ryll/src/session.rs`,
   `let (_resize_tx, resize_rx)`), it passes no clipboard backend, and
   the control socket's verbs are `hello`, `status`, `send_key`,
   `paste`, `screenshot`, `subscribe` and `unsubscribe`. `paste` types
   keystrokes; it does not use the clipboard. `status` and the
   `agent_connected` event do report the agent.
7. **Kerbside lets all of it through.** Its L1 allowlist passes any
   opcode `message_names` names, its main-channel size cap is 16 MiB,
   and it does not look inside agent payloads. It offers clients
   `MAIN_AGENT_CONNECTED_TOKENS` and forwards the client's caps to the
   backend.
8. **xrdp-chansrv bridges CLIPBOARD only**, and only while the RDP
   clipboard channel is up (`sesman/chansrv/clipboard.c:390`,
   `:410-414`; `chansrv.c:406`, `:1414`):
   - when the RDP client copies, chansrv takes ownership of CLIPBOARD
     and offers `UTF8_STRING`, `STRING`, and images or files when the
     client has them;
   - when any other X client takes ownership, chansrv asks it for
     `TARGETS` and announces the formats to the RDP client. It ignores
     its own ownership changes and never re-grabs in response to
     another owner, so no loop forms inside the X server;
   - it supports INCR both ways.
   A loop *through the client machine* is plausible when the RDP client
   and ryll share one desktop clipboard, as on the operator's Mac: each
   bridge's ownership change reaches the Mac's pasteboard through the
   other client, and comes back. Decision 5 is built around this.
9. **The X11 backend has room for a fifth connection.** Four threads
   each own a `RustConnection` (capture, input, cursor, damage); the
   event-driven ones block on events and are stopped by a `Waker`
   (`platform/x11/mod.rs:136-169`). `platform/` carries no SPICE or
   ryll types (`AGENTS.md`). There is no display-control or clipboard
   trait yet; `ARCHITECTURE.md:143` says so. x11rb's `randr` feature is
   not enabled (`andris/Cargo.toml:34`).
10. **A resize already reaches the client.** The damage thread sees the
    root's `ConfigureNotify`, the display channel replaces the surface,
    and andris sends `MONITORS_CONFIG` with one head to a client that
    asked for it (`server/display.rs:458-562`). A resize andris makes
    itself takes the same path, so the display side needs no change.
11. **spice-server's agent plumbing, for behaviour:** `MAIN_INIT`
    carries a window of 10 tokens; each `AGENT_DATA` from the client
    costs one, and running out is a token violation that disconnects
    the client; tokens come back in batches of 5. Each `AGENT_DATA` is
    at most 2048 bytes including the 20-byte message header on a
    message's first chunk. spice-vdagent replies `REPLY` with success
    to `MONITORS_CONFIG` as soon as it arrives, before applying it.
    spice-gtk always appends the physical-size table, so andris must
    accept it.

This plan corrects the master plan at source: open questions 1 and 2,
the phase 7 section, the success criterion on resizing, and Future
work. Later steps need not redo that.

## Decisions

1. **Resize through RandR where the X server allows it; on xorgxrdp,
   say no and leave the size alone.** Decided with the operator on
   2026-10-14. Andris applies a monitors config with RandR: it rounds
   the size as the client sent it, clamps it to the server's size
   range, finds or adds a mode, and changes the screen size and the
   CRTC in the order that keeps the CRTC inside the screen. When the
   server refuses, as xorgxrdp always does, andris logs once per
   connection that the X server does not allow clients to resize it,
   replies to the client with an error, and changes nothing. The
   desktop then follows the RDP client's size, as it does today.
   Generic RandR is still worth having: Xvfb and Xvnc sessions resize,
   and the e2e checks can exercise the path. The master plan's
   success criterion is amended to match.
2. **Ryll gains a headless clipboard and resize first.** Decided with
   the operator on 2026-10-14, following phases 2 and 3: a ryll pull
   request lands first, and andris bumps its pin to it. The bump is
   needed anyway, for the agent reassembler (survey finding 4). Without
   it, real ryll is exercised only in the acceptance run, and a
   mismatch in token pacing or chunking would surface there rather than
   in `make e2e`.
3. **The agent is always connected.** Andris is inside the session, so
   it announces the agent in `MAIN_INIT` (`agent_connected: 1`, a
   window of 10 tokens) and never sends `AGENT_CONNECTED` later. It
   grants tokens as spice-server does, and returns them as it consumes
   each chunk. A client that sends `AGENT_DATA` with no token left, or
   a chunk over 2048 bytes, or an agent message over the reassembler's
   cap, has its main channel closed, as phase 4 closes on other
   malformed input. Andris sends its own messages in chunks of at most
   2048 bytes, one per token the client granted, queuing when it has
   none, with token counts kept in saturating arithmetic.
4. **Text on CLIPBOARD only.** Ryll carries only `UTF8_TEXT` on
   CLIPBOARD; macOS has no PRIMARY; chansrv bridges only CLIPBOARD. So
   andris bridges CLIPBOARD as UTF-8 text, offering `UTF8_STRING`,
   `STRING` (converted from Latin-1 when that is all the owner has)
   and `text/plain;charset=utf-8`, with INCR both ways. It announces
   `CLIPBOARD_BY_DEMAND`, `CLIPBOARD_SELECTION`, `MONITORS_CONFIG`,
   `REPLY` and `GUEST_LINEEND_LF`, never `CLIPBOARD_GRAB_SERIAL`. A
   request for another selection or type is answered with an empty
   clipboard. The master plan named PRIMARY too; it moves to Future
   work, with images.
5. **Fetch eagerly, and drop echoes by content.** This is the decision
   a reviewer is most likely to argue with, because SPICE's clipboard
   is by demand and this is not. When the client grabs with UTF-8 text,
   andris requests the text at once, and takes X ownership only when it
   has it, serving every X request from memory. When an X client takes
   ownership, andris fetches its text at once, and grabs on the SPICE
   side only if the text differs from the last text that crossed the
   bridge in either direction. The reasons:
   - It breaks the loop through the Mac (survey finding 8). Each
     bridge's ownership change comes back through the other client
     carrying the same text; comparing content stops it on the first
     repeat, where comparing owners or timestamps cannot.
   - An X requestor, chansrv among them, never waits on a round trip
     to the SPICE client, so chansrv's timeouts never matter.
   - The cost is a transfer per copy, whether or not it is pasted. For
     text, capped at 16 MiB, that is small, and ryll is already eager
     on its side.
6. **RandR belongs on the capture thread; the clipboard gets its own.**
   Display control is request and reply, rare, and the capture thread
   already answers `size()`, so a `SetSize` request joins its queue.
   The clipboard is event-driven (`SelectionRequest`,
   `SelectionNotify`, XFixes selection events, INCR property events),
   so it becomes a fifth thread with its own connection, built like the
   cursor thread: a wake window, a `Waker` on drop, and a `GoneSender`
   clone.
7. **RANDR becomes optional at startup.** An X server without RANDR, or
   with a version older than 1.2, still serves; andris refuses resizes
   on it, as on xorgxrdp. XFIXES is already required.
8. **The platform seams speak in platform types.**
   `DisplayControl::set_size(Size) -> Result<Resize>`, where `Resize`
   is `Applied`, `Unchanged` or `Refused(reason)`, may block and is
   called through `spawn_blocking`. `Clipboard` has
   `changes() -> watch::Receiver<…>` carrying the desktop's text (or
   none), and `set(text)`, which never blocks. No SPICE type crosses
   into `platform/`.

## Execution

Implementation is done by sub-agents briefed from this plan. The
management session reviews each diff and makes each commit. For the
ryll step, the rules are ryll's: its `make lint` and `make test`, and
its conventions in its `AGENTS.md`.

- Every andris commit must leave `make lint` and `make test` passing.
- Cargo runs only through the `Makefile`, never on the host.
- No code is translated from spice-server, spice-gtk, spice-vdagent,
  x11spice, xrdp or xorgxrdp. Their behaviour is cited from the survey.
- Every step adds tests for what it adds.
- **No sub-agent connects to, signals, resizes, reads the clipboard of,
  or injects into the host's `:10` session**, nor touches its xrdp,
  sesman or chansrv. The host shell has `DISPLAY=:10.0`: unset
  `DISPLAY` and `XAUTHORITY` for every X command. All xrdp testing
  happens in the test-bed container, on `:50`.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Complete | Commit the phase 6 close-out and this plan; create the ryll worktree. See brief 0. |
| 1 | high | opus | none | Complete | Spike: xorgxrdp's refusal, Xvfb's RandR, and chansrv's clipboard, on the test bed. Gated. See brief 1. |
| 2 | high | opus | none | In progress | Ryll: headless clipboard, and control verbs for the clipboard and resize. Ryll PR. See brief 2. |
| 3 | medium | sonnet | none | Complete | Andris: bump the ryll pin to step 2's merge. See brief 3. |
| 4 | high | opus | none | Complete | The agent protocol on the main channel, the two seams, and the synthetic platform's side. See brief 4. |
| 5 | high | opus | none | In progress | X11 display control through RandR. See brief 5. |
| 6 | high | opus | none | Not started | The X11 clipboard thread. See brief 6. |
| 7 | high | opus | none | Not started | End-to-end checks: `make e2e` and `make e2e-xrdp`. See brief 7. |
| 8 | medium | sonnet | none | Not started | Documentation, and the xorgxrdp feature request drafted. See brief 8. |
| 9 | medium | management | none | Not started | Operator acceptance on `:10`, by hand. See brief 9. |
| 10 | medium | management | none | Not started | Push audit, pull request, queue. See brief 10. |

Steps 1 and 2 may run in parallel. Step 3 needs step 2's ryll pull
request merged. Step 4 needs step 3. Steps 5 and 6 both touch
`platform/x11/mod.rs`, so they run one after the other, after step 4.
Step 7 needs steps 5 and 6, and its RandR checks depend on step 1's
Xvfb result. Steps 8, 9 and 10 follow in order.

### Brief 0: commits and the ryll worktree

Make two commits, with the operator's permission:
- the phase 6 close-out: phase 6 Complete with `andris 498a72c (#12)`,
  phase 6's step 7 Complete, ryll#527 in its Result, and the packaging
  pivot to PyPI wheels with `andris install-unit` in phase 10, as the
  operator asked on 2026-10-13;
- this plan, with the master plan corrections listed under the survey,
  and the new human-review phase the operator asked for on 2026-10-14.

Create the ryll worktree from an up-to-date `origin/develop`:
`git -C ../ryll worktree add -b x11-desktop-phase-07-headless-agent
../ryll-wt-p07 origin/develop`.

### Brief 1: spike on the test bed

**Goal:** settle, live, three things the plan relies on, and record them
in the Result section. Read-only with respect to andris's code.

Use `scripts/xrdp-testbed/` and the `andris-xrdp-testbed` image, never
the host's `:10`. Inside the container:

1. **xorgxrdp's refusal.** With FreeRDP connected to the `:50` session,
   run `xrandr --fb 1280x720` and `xrandr --output rdp0 --mode` with an
   added mode. Record the errors (expect `BadMatch` from
   `RRSetScreenSize`, and a CRTC change that reports success and does
   nothing). Repeat with FreeRDP disconnected (the session survives,
   `KillDisconnected=false`). Record `xrandr --verbose`: output names,
   CRTCs, the size range.
2. **Xvfb's RandR**, at the version in the `andris-dev` image (the one
   `make e2e` uses). Can a client change the screen size: with
   `xrandr --fb`, with `--newmode`, `--addmode` and `--output … --mode`,
   to a size larger and smaller than the start? Does the root's
   `ConfigureNotify` arrive? If Xvfb cannot resize, say so: step 7 then
   checks only the refusal path against Xvfb, and the applied path is
   covered by a test of the RandR sequence against whatever the spike
   finds can resize, or by unit tests of the plan alone.
3. **chansrv's clipboard.** With FreeRDP connected, on FreeRDP's own
   Xvfb set the clipboard (`xclip`, installed in the test bed if
   absent) and read CLIPBOARD on `:50`; then set CLIPBOARD on `:50`
   with `xclip` and read it on FreeRDP's display. Record which targets
   chansrv offers and asks for, and how long it waits for a slow owner
   (an owner that delays `SelectionNotify`; a few lines of Python with
   `python3-xlib` are enough). This tells step 7 what an interleaving
   check can observe.

Gate: report the three results to the management session before steps
5 and 7 begin. A result that contradicts survey finding 1 or 8 reopens
decisions 1 or 5 with the operator.

### Brief 2: ryll, headless clipboard and resize

Work in `../ryll-wt-p07`, under ryll's `AGENTS.md`, `make lint` and
`make test`. This is ryll's change; andris's plan only describes it.

**Goal:** ryll's headless mode can carry clipboard text both ways and
send a resize, driven and observed through the control socket.

- **An in-memory clipboard backend** implementing ryll's clipboard
  trait (`get_text`/`set_text`, `ryll/src/clipboard.rs`), passed to the
  main channel in headless mode where `session.rs` now passes `None`.
- **Control verbs**, documented in `docs/control-socket-protocol.md`
  with the protocol's minor version raised:
  - `clipboard_set {text}`: sets the in-memory clipboard, so the next
    poll grabs on the guest;
  - `clipboard_get`: returns the in-memory clipboard's text, or null;
  - a `clipboard_changed` event, sent when the guest's text arrives;
  - `resize {width, height}`: sends a monitors config, through the
    resize sender headless mode creates and never uses.
- **Check the suspected gap** in survey finding 5: a resize asked for
  before the agent connects should be sent once it does. Fix it if it
  is real, with a test.
- Tests for each verb, at the level ryll tests its other verbs.

Then ryll's own push audit, and ask the operator before opening the
pull request. Its body says andris phase 7 needs it. The management
session waits for the merge before step 3.

### Brief 3: bump the ryll pin

**Goal:** andris builds against ryll at step 2's merge commit.

- Move all three `rev =` lines in `andris/Cargo.toml` together (the
  comment there says so), and update `Cargo.lock` with `make` targets
  only.
- Fix whatever the protocol crate's churn breaks (survey finding 4
  names `common.rs`). Change no behaviour.
- `make e2e` and `make e2e-xrdp` still pass: the Makefile builds
  headless ryll at the pinned revision.
- `deny.toml` needs no change for a git dependency on the same
  repository; check.

Commit subject: "Bump ryll to bring in the agent reassembler."

### Brief 4: the agent protocol and the seams

**Goal:** andris speaks the vdagent protocol on its main channel, over
two new platform seams, with the synthetic platform implementing both.

- **Seams** (`platform/mod.rs`), per decision 8: `DisplayControl` and
  `Clipboard`, added to `Platform`. No SPICE types. Document their
  blocking rules as the module's other traits do.
- **Synthetic platform:** `set_size` resizes the pattern (it already
  has `set_size` hooks for tests); a test hook makes it refuse, as
  xorgxrdp does. The clipboard is in memory, with a test hook to set
  the desktop's text as if another X client had copied.
- **Main channel** (`server/main_channel.rs`, a new `server/agent.rs`
  for the agent state):
  - `MainOpcode` gains `AgentConnectedTokens`, `AgentData`,
    `AgentToken` (and `AgentDisconnected` only if used); the existing
    test that ryll names every variant covers them;
  - `MAIN_INIT` carries `agent_connected: 1` and 10 tokens
    (decision 3);
  - `AGENT_START` sets the client's credit; `AGENT_DATA` is checked
    against the window and the 2048-byte chunk limit, fed to ryll's
    `AgentReassembler`, and its token returned; violations close the
    channel;
  - outgoing messages are chunked at 2048 bytes and sent one per
    client token, queued when there is none;
  - on `AGENT_START`, andris announces its capabilities with
    `request: 1` (decision 4), and answers a client announce that
    asks for one;
  - `MONITORS_CONFIG`: validate (at least one monitor; sizes positive
    and within the protocol's limits, with no overflow in
    `w * h * 4`; accept the physical-size table), apply the first
    monitor through `DisplayControl` on `spawn_blocking`, and reply
    `REPLY` with success for `Applied` or `Unchanged` and an error
    for `Refused`, logging the refusal once per connection;
  - the clipboard, per decision 5: on a client grab offering
    `UTF8_TEXT`, request it; on the data, if it differs from the last
    text that crossed, `Clipboard::set` it; on a desktop change that
    differs, grab on the client side; on a client request for
    `UTF8_TEXT`, answer from the desktop text held; on anything else,
    an empty clipboard; on a client release, nothing further. Text is
    capped at 16 MiB in both directions; larger is dropped with a
    warning.
- **Tests** with `TestClient` over the synthetic platform: the announce,
  token accounting both ways (including a violation that closes the
  channel), chunking of a message larger than one chunk, reassembly of
  one the client splits, a resize applied, unchanged and refused, a
  clipboard round trip each way, and an echo that is dropped. Update
  `tests/main_channel.rs:61-62` and `:150`.

### Brief 5: X11 display control through RandR

**Goal:** `X11Platform` implements `DisplayControl` with RandR 1.2+.

- Enable x11rb's `randr` feature. In `checks.rs`, RANDR is optional
  (decision 7): record whether it is present at 1.2 or later.
- On the capture thread (decision 6), a `SetSize` request:
  - without RANDR, `Refused`;
  - the size clamped to `RRGetScreenSizeRange`, and `Unchanged` if it
    is the current size;
  - choose the output: the primary output if set, else the first
    connected output with a CRTC;
  - find a mode of exactly that size, or create one named for andris
    and the size, and add it to the output;
  - when growing, set the screen size and then the CRTC; when
    shrinking, the CRTC and then the screen size, so the CRTC always
    fits; physical size in millimetres at 96 DPI;
  - an X error at any point is `Refused`, with the X error named.
    Never trust `RRSetCrtcConfig`'s Success: on xorgxrdp it changes
    nothing, so a shrink done CRTC-first would look applied. Read the
    screen and CRTC geometry back, and treat a size that did not
    change as `Refused`. Restore what was changed before the failure,
    as far as RandR allows;
  - on a refusal, delete from the output and destroy every mode the
    attempt created; step 1 saw each refused attempt otherwise leave
    a mode on `rdp0` for the operator to find. Remove modes andris
    added that are no longer in use after an applied resize too.
- The resize then reaches the client through the existing
  `ConfigureNotify` path (survey finding 10); add nothing there.
- Tests: Xvfb resizes within its `-screen` size and refuses above it
  (step 1), so both paths can be exercised against it: shrink and grow
  back for `Applied`, and a size above the start for `Refused`.

### Brief 6: the X11 clipboard thread

**Goal:** `X11Platform` implements `Clipboard` on a fifth connection,
per decisions 4, 5 and 6.

- A thread built like the cursor thread: its own `RustConnection`, an
  `InputOnly` window that owns the selection and receives events, the
  wake window and `Waker`, `lost()` on a dead connection.
- XFixes `SelectSelectionInput` on CLIPBOARD. On a new owner that is not
  andris's window: convert `TARGETS`, then `UTF8_STRING` (or `STRING`,
  converted from Latin-1), into a property on andris's window; follow
  INCR; cap at 16 MiB; publish the text on the `watch`. On no owner,
  publish none.
- `set(text)` (from a queue, with a wake): take ownership with a real
  timestamp (from a zero-length property change, not `CurrentTime`),
  and serve `SelectionRequest` for `TARGETS`, `TIMESTAMP`,
  `UTF8_STRING`, `STRING` and `text/plain;charset=utf-8`, using INCR
  above the server's maximum request size; refuse other targets and
  `MULTIPLE`. On `SelectionClear`, stop serving.
- A requestor that never finishes an INCR transfer must not stall the
  thread: per-transfer state with a timeout.
- **Every conversion andris asks for has a timeout too** (10 s), and a
  conversion that times out publishes no text. Step 1 found that
  chansrv keeps CLIPBOARD after the RDP client disconnects, and that
  requests to it then hang for ever; andris will meet that owner at
  startup and on every eager fetch while RDP is away.
- Tests against Xvfb in `make e2e`, with `xclip`, which neither the
  dev image nor the test bed has: add `xclip` and `x11-xserver-utils`
  (for `xrandr`) to both Dockerfiles: set on X and read through andris's seam,
  set through the seam and read with `xclip`, a text over the INCR
  threshold each way.

### Brief 7: end-to-end checks

**Goal:** the agent is checked through real ryll, using step 2's verbs.

- `make e2e` (Xvfb): `clipboard_set` in ryll reaches `xclip -o` on the
  display; `xclip -i` on the display reaches ryll's `clipboard_get`
  (wait for `clipboard_changed`); a text over one agent chunk, and one
  over the INCR threshold, both ways. `resize`: a shrink and a grow
  back within Xvfb's `-screen` size reach ryll as new surfaces; a size
  above it is refused, logged once, and leaves the surface unchanged.
- `make e2e-xrdp`, new interleaving checks:
  - **X8**, RDP to SPICE: text copied on FreeRDP's display arrives in
    ryll's clipboard;
  - **X9**, SPICE to RDP: `clipboard_set` in ryll arrives on FreeRDP's
    display;
  - **X10**, no loop: a helper on FreeRDP's display stands in for the
    Mac's shared pasteboard. It watches FreeRDP's CLIPBOARD with
    XFixes, feeds new text to ryll's `clipboard_set`, and owns
    FreeRDP's CLIPBOARD with the text of each `clipboard_changed`,
    ignoring its own ownership. After one copy each way, count the
    grabs andris logs over 5 s and require none beyond the first.
    Step 1's model of this loop ran at about 750 bridges a second
    without content comparison, so a regression cannot hide;
  - **X11**, resize from SPICE: ryll's `resize` is refused, andris
    logs it once, ryll's surface stays at the RDP size, and an RDP
    resize afterwards still reaches ryll.
- chansrv fetches only when the RDP side pastes, and FreeRDP caches
  what it fetched, so a second read on FreeRDP's display proves
  nothing new; each check copies fresh text.
- Step 1's spike scripts, in the management session's scratchpad
  (`spike/loop.py`, `owner.py`, `setup.sh`), are a starting point; the
  management session copies them in. python-xlib copies its event
  classes, so compare `type(event).__name__`, not `isinstance`.
- Record each result, as phase 6 did for X1 to X7.

### Brief 8: documentation

- `docs/running.md`: the agent, copy and paste (text, CLIPBOARD), and
  resizing, including that it needs an X server that allows it;
  replace "No guest agent" (`:296-297`) and update the main channel's
  row (`:220`).
- `docs/xrdp.md`: on xrdp the desktop follows the RDP client's size
  and a SPICE resize is refused, and why; the clipboard works with RDP
  connected, through chansrv, and what to expect with both clients on
  one machine.
- `docs/index.md` and the README's one-line limitation, if they still
  say there is no clipboard.
- `ARCHITECTURE.md:143`: the display-control and clipboard seams, and
  the fifth X connection.
- Draft, in the Result section, an upstream feature request for
  xorgxrdp: let an X client resize the screen through RandR when no
  RDP client is connected, or tell xrdp of a size an X client asks for.
  The operator decides whether to file it.

### Brief 9: operator acceptance on `:10`

With the operator, on the real session, using the Mac's ryll from
`develop` (or the release that carries step 2), with the RDP client
connected on the same Mac:
- copy text on the Mac, paste into a terminal in the SPICE session;
  copy in the session, paste on the Mac;
- watch andris's log for repeated grabs for 30 s after each: decision
  5's loop check;
- resize the ryll window: the desktop does not resize, andris logs
  the refusal once, and the RDP session is unaffected.

### Brief 10: push audit

Run `PUSH-AUDIT.md` over `origin/develop...HEAD` in two waves, as
phases 5 and 6 did, fix what is blocking, record what is declined, and
open the pull request when the operator asks.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| The clipboard loops between the RDP client and ryll on the Mac | Decision 5's content comparison. X10 checks it in the test bed where it can, and step 9 on the Mac. The management session reads andris's grab logs in both |
| A sub-agent touches the operator's clipboard or resizes `:10` | The Execution rules; every test-bed script's marker-file guard; the management session checks every new script's display handling |
| Token accounting stalls ryll, or closes a well-behaved client | Step 4's tests cover a message larger than the window, and step 7 checks a multi-chunk text through real ryll |
| The pin bump drags in protocol-crate churn that changes behaviour | Step 3 changes no behaviour, and `make e2e` and `make e2e-xrdp` must pass on it before step 4 starts |
| Ryll's pull request is slow to merge, blocking steps 3 onwards | Step 1 runs meanwhile. If the wait is long, step 4 can begin on a temporary pin to the ryll branch, which must be replaced by the merge commit before step 10 |
| A slow or hostile X requestor stalls the clipboard thread | Per-transfer state with a timeout, in brief 6 |
| Eager fetching moves large texts nobody pastes | The 16 MiB cap, and text only |
| RandR changes leave the session in a mode the operator did not choose, on a server that does allow it | Andris only resizes when a client asks, restores on failure, and removes the modes it added |

## Definition of done

- **The checks pass.** `make lint test e2e` passes, and `make e2e-xrdp`
  passes locally with X8 to X11 recorded in the Result section.
- **Ryll's half has merged.** The ryll pull request's merge commit is
  the pin in `andris/Cargo.toml`, and the master plan's `Merged` cell
  names both pull requests.
- **The agent is announced.** `tests/main_channel.rs` asserts
  `agent_connected == 1` and a non-zero token window.
- **No grab serial.** `grep -rn GRAB_SERIAL andris/src` finds only a
  comment saying why it is not announced.
- **Refusal is graceful.** A test refuses a resize on the synthetic
  platform and asserts an error reply and one log line, and check X11
  shows the same against xorgxrdp on `:50`.
- **No echo.** A test feeds the same text back through the other side
  and asserts no second grab.
- **Documentation.** `grep -n "No guest agent" docs/` finds nothing;
  `docs/xrdp.md` says the desktop follows the RDP client's size;
  `grep -n "phase" docs/running.md docs/xrdp.md` finds nothing.
- **Deferred work is tracked.** PRIMARY and clipboard images are in the
  master plan's Future work; the xorgxrdp request is drafted; every
  ryll gap found has an issue.

## Result

Filled in as the steps complete.

**Departure: step 3 ran before step 2 merged.** The agent reassembler
andris needs was already on ryll's `develop`, so step 3 pinned
`7e3705e` (ryll's `develop` on 2026-10-14) rather than waiting for the
ryll pull request, and step 4 could start at once. A second bump, to
that pull request's merge commit, lands before step 7, whose checks
need the new control-socket verbs.

**Step 3: the pin bump.** `502199e` to `7e3705e`. The only break was
ryll#510's `ReceiveBuffer`, which replaced the free `take_message`;
andris's channel reader now reads into a 16 KiB chunk and appends it,
since `ReceiveBuffer` cannot take tokio's `read_buf`. Behaviour is
unchanged, and framing many queued messages is now linear rather than
quadratic. `make lint test e2e e2e-xrdp` passed.

**Step 4: the agent protocol.** Departures from brief 4, each taken in
review:
- `AgentConnectedTokens` is not added to `MainOpcode`: the agent is
  connected from `MAIN_INIT` (decision 3), so it is never sent.
- Tokens come back in batches of five, and only once andris has
  handled everything it has read, so that a client sending past its
  window in one burst is caught rather than refilled.
- The reassembler is capped at 16 MiB plus 4 KiB. A clipboard message
  over it is dropped with a warning; any other closes the channel. A
  monitors config that decodes but asks for an impossible size gets an
  error `REPLY`; one that does not decode closes the channel.
- Added: desktop text present before the client announces is offered
  once it does; losing the desktop's text releases a standing grab;
  grabs go only to a client that announced `CLIPBOARD_BY_DEMAND`; the
  outgoing queue is capped at about 32 MiB.
- `X11Platform` refuses resizes and has an empty clipboard until steps
  5 and 6 replace them.

**Step 1: the spike** (2026-10-14, about 12 minutes, all in the test-bed
container; nothing touched `:10`). It confirmed survey findings 1 and 8
and reopened no decision. A throwaway image, `andris-xrdp-spike`, added
`xclip` and `x11-xserver-utils`, which neither image has.

- **xorgxrdp refuses every resize.** RandR 1.6, one output `rdp0` on
  one CRTC, size range 256x256 to 16384x16384. With FreeRDP connected
  or killed, `xrandr --fb` and `--output rdp0 --mode` fail with
  `BadMatch` from `RRSetScreenSize`; `--newmode` and `--addmode`
  succeed and leave the mode on `rdp0`; a raw `RRSetCrtcConfig`
  replies Success and changes nothing; no `ConfigureNotify` arrives.
  Brief 5 now reads geometry back and cleans up its modes.
- **Xvfb resizes, up to its start size.** Xvfb 21.1.16 started as
  `make e2e` starts it: RandR 1.6, one output `screen`, size range
  1x1 to the `-screen` size. Shrinking and growing back apply,
  arbitrary created modes included, each with a root
  `ConfigureNotify`; above the start size, `RRSetScreenSize` fails
  with `BadValue`. Brief 7 tests both paths against it.
- **chansrv:** xfreerdp3 redirects the clipboard by default. chansrv
  offers `TARGETS TIMESTAMP MULTIPLE STRING UTF8_STRING`, asks a new
  owner for `TARGETS` at once and for `UTF8_STRING` only when the RDP
  side pastes, and never timed out on an owner that delayed its
  replies by up to 100 s. **New:** after the RDP client disconnects,
  chansrv keeps CLIPBOARD, and requests to it hang until reconnect.
  Brief 6 now bounds every conversion.
- **The loop is real, and content comparison stops it.** A model of
  the Mac's shared pasteboard, bridging FreeRDP's display and `:50`
  directly, made 4513 bridges in 6 s from one copy without
  comparison, and exactly one per copy with it, in both directions.
  Brief 7's X10 is built on that model.

## Back brief

Before executing any step of this plan, please back brief the operator
as to your understanding of the plan and how the work you intend to do
aligns with that plan.
