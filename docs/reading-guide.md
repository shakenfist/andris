# Reading andris

An order to read andris's code in, and what to look at in each file. It
is for someone who knows Rust and roughly what SPICE is, and who has not
read andris before. It starts with the documents, goes from the binary's
entry point through the server and its channels, then crosses the
platform seam to the X11 and PulseAudio backends, and ends with the
tests, the build and the tooling around them.

Each entry says what a file is for, its main types and functions, what
to look at closely, and what to read beside it. "Look at" is the part
worth slowing down for: the invariants a file keeps, the places where a
mistake would be subtle, and where it talks to ryll's crates or to the X
server. Where a file uses ryll's `shakenfist-spice-protocol` or
`shakenfist-spice-compression`, the entry names the types, so that you
can open them alongside.

**Using it with the review tooling.** `./tools/review-tracking.sh next`
offers the files in this order: every file with a heading below, top to
bottom, and then the rest of the repository by file type and path. So
the last three sections, scripts, CI and the rest, are a map of what
remains rather than a sequence.

Most source files end in a `#[cfg(test)]` module, often as long as the
code above it. Read the code first, then skim the test names: they are
usually the clearest statement of what the code promises.

**Conventions.** Some hold everywhere, and are worth knowing before you
start:

- `#![deny(unsafe_code)]` holds crate-wide. Only `platform/x11/shm.rs`,
  `platform/pulse/ffi.rs` and, with the `qemu` feature,
  `platform/qemu/shm.rs` opt out: `ffi.rs` with an inner attribute, and
  each `shm` with one on its `mod` line in its platform's `mod.rs`.
- The server (`andris/src/server/`) knows SPICE and nothing of X11. The
  platform (`andris/src/platform/`) knows the desktop and nothing of
  SPICE. Keys cross between them as Linux evdev codes.
- Platform methods are synchronous. One that may block is called from
  async code through `tokio::task::spawn_blocking`.
- Each channel runs in one task that owns its socket. A session's end
  drops that task's future, so cleanup the desktop is owed, such as
  releasing held keys, lives in `Drop` guards.
- Errors are `anyhow`, with `.context()` that says what was being done.
  `clippy::unwrap_used` is on for everything but tests.

**How a frame and a key press travel.** A frame starts with the X
server. The damage thread (`platform/x11/damage.rs`) reads XDamage
events on the root window and reports each rectangle to a `DamageHub`
(`platform/damage.rs`), which rounds it out to 64x64 tiles in every
subscriber's pending `Region`. The display channel (`server/display.rs`)
holds one `DamageSubscription`. When damage is pending, the frame
interval has passed and the ack window (`server/channel.rs`) has room,
it takes the damage and asks the platform to capture those tiles, on the
blocking pool. The capture thread (`platform/x11/capture.rs`) reads them
with MIT-SHM (`platform/x11/shm.rs`) or `GetImage`. The update encoder
(`server/updates/`) compares the capture with its shadow of what the
client already shows, merges the changed tiles into at most 32
rectangles, and encodes each as a `DRAW_COPY`, as JPEG, LZ4 or a bitmap.
A `MARK` ends the frame. On the side, the heat map (`updates/heat.rs`)
watches for an area that keeps changing like a picture, and
`updates/stream.rs` turns it into an H.264 stream.

A key press goes the other way. The inputs channel (`server/inputs.rs`)
receives PC set-1 scancode bytes, and its `KeyDecoder`
(`server/keymap.rs`) turns them into evdev codes. A `HeldInput` guard
records what is down, and hands an `InputEvent` to the platform's
`InputSink`, which only queues it. The X11 input thread
(`platform/x11/input.rs`) maps the evdev code to an XKB key name, and
that name to the server's keycode (`platform/x11/keymap.rs`), then sends
it with XTEST.

## Orientation

Start with what andris is and how it is run, so that the code has a
shape to hang on.

### `README.md`

The pitch: a SPICE server for a Linux desktop that is already running,
attached as an ordinary X client, and served to ryll directly or through
kerbside. Note what it refuses to be: it does not start a desktop, does
not support Wayland, and serves one client at a time. The status
paragraph lists what works today.

### `ARCHITECTURE.md`

The map. Its component table says where each piece lives, and its three
walk-throughs (a frame, a sound, a key) are the long form of the section
above. Read the "Relationship to ryll and kerbside" section closely: it
explains why andris sends only message types ryll models, and why wire
types and encoders live in ryll's crates rather than here. Several files
below exist to keep those promises.

### `docs/index.md`

The documentation index. It is short; read it to know what else exists,
then move on.

### `docs/running.md`

The operator's view, and the clearest statement of behaviour outside the
code's own comments. "What a client gets" describes the display
channel's frames, JPEG and the lossless refresh, video streams and their
numbers, and what is not supported. "Audio" and "Metrics" do the same
for sound and for the periodic log line. Keep it open beside
`server/display.rs` and `server/updates/stream.rs` later: the code's doc
comments refer to it.

### `docs/xrdp.md`

Running andris as a systemd user unit beside xrdp, and what happens when
an RDP client and a SPICE client share one desktop: input, clipboard,
resizing and sound. "When the session ends" explains the exit statuses
that `main.rs` and the unit agree on.

### `docs/reading-guide.md`

This guide. It is in the queue so that it is reviewed like any other
file: if an entry here is wrong about the code, fix the entry.

## The binary

How andris starts, what it accepts, and how it stops.

### `andris/src/main.rs`

The thin binary. It parses the command line, runs a subcommand
(`install-unit`) before any logging or runtime exists, sets up `tracing`
on stderr, and hands a validated `Config` to the library. Look at the
exit statuses: a refused configuration exits 2, which the systemd unit's
`RestartPreventExitStatus=2` relies on, and a runtime failure, the
desktop going among them, exits 1. Read beside
`packaging/systemd/andris@.service`.

### `andris/src/lib.rs`

The library root and the process's lifetime. `run()` builds the platform
the configuration names, attaches the sound source, and starts a
multi-threaded tokio runtime. `serve()` binds the listeners, and
`serve_until()` waits for SIGINT, SIGTERM or the platform's
`GoneSignal`. Look at the two different stops: a signal waits up to
`STOP_GRACE` so held keys are released into the desktop, while a desktop
that has gone waits only `GONE_GRACE` and then fails, so the process
exits non-zero. Note also that signals are listened for before the
listeners are bound, and that libpulse is loaded before the display is
opened, so a missing library is reported either way.

### `andris/src/config.rs`

The command line (`Cli`, `Args`, the `Command` subcommand) and
`Args::validate()`, which turns it into a `Config`. Every file andris
needs is read here, so a bad one is a refused configuration rather than
a runtime failure. Look at the cross-option rules: the three TLS options
go together, a plain listener must be loopback, and `--audio-source`
conflicts with the synthetic platform. Look closely at
`open_private_file()` and `check_private()`: the ticket file and TLS key
must be regular files owned by andris's effective uid with no group or
other access, checked on the open descriptor so that the file checked is
the file read. The effective uid comes from `/proc/self/status` to avoid
an `unsafe` call. `Ticket` hides its bytes from `Debug`.

### `andris/src/install.rs`

`andris install-unit`: renders the systemd user unit with the running
binary's absolute path and installs it, with a ticket if there is none.
It never runs systemctl. Look at the refusal paths: running as root
(`refuse_root`), a binary deleted since it started, and
`quote_executable()`, whose doc comment records which characters systemd
257 accepts in an executable path and why `"`, `'`, `\` and control
characters are refused rather than escaped. Look also at how files are
written: the unit through a sibling file and `rename()`, so a symbolic
link into a checkout is replaced rather than written through; the ticket
with `create_new` and mode 0600, so a concurrent writer is never
clobbered. Read beside `packaging/systemd/andris@.service`, which is
embedded with `include_str!`.

## The server core

Listening, linking, sessions, and the framing every channel shares.

### `andris/src/server/mod.rs`

The listeners and the task model. `Server::bind()` and `spawn()` start
one accept task per listener; each connection gets one task that links
it and then serves its channel. Look at the limits on unauthenticated
peers in `admit()`: a connection holds one of `MAX_HANDSHAKES` permits
and one of `MAX_HANDSHAKES_PER_PEER` slots for its source address (IPv6
counted by /64) until its handshake ends, and a plain listener that only
redirects to TLS has permits of its own. `LogThrottle` keeps a peer from
flooding the log. Look at `ServerHandle::stop()`: it waits for every
task's `mpsc` sender to drop, then reads the lock state on the blocking
pool purely as a barrier, because that read runs after every queued
input event, the releases included. `run_channel()` races a channel
against a later link of its type, and against its session closing its
secondary channels after a replacement. Ryll types: `link::{SpiceStream,
read_link_mess, send_need_secured}`.

### `andris/src/server/link.rs`

The link handshake and ticket authentication, in kerbside's order.
`handshake()` reads the link message, refuses a client without
`AUTH_SELECTION` and `MINI_HEADER`, a channel andris does not serve, or
a secondary channel that does not name the current session, then
generates a fresh RSA key per connection, reads the ticket and answers.
Look at the link-and-ticket check: every stage is under a time limit
(`HandshakeTimeouts`), key generation runs on the blocking pool under a
semaphore whose permit moves into the task, `ticket_matches()` compares
in constant time, and a wrong ticket is answered only after a delay
while the handshake slot is still held. A main channel's session is
begun before the client hears it is in. The capability words andris
offers mirror kerbside's offer per channel. Ryll types:
`link::{SpiceLinkReply, generate_ticket_keypair, read_auth_ticket,
read_link_mess, send_auth_result, send_link_reply}`,
`reader::LinkError`, `ChannelType`, `SpiceError`, `capabilities`.

### `andris/src/server/tls.rs`

`load_server_config()` builds a rustls `ServerConfig` from a PEM chain
and key. Short. The key goes through `open_private_file()` from
`config.rs`; the certificate does not need to be private.

### `andris/src/server/session.rs`

The `SessionRegistry` holds the one current `Session`. Look at the
single-client replacement: `begin()` does not cancel the old session, it
marks it replaced, because a client dropped without a word would
reconnect and take the desktop straight back. The old main channel then
says goodbye (see `main_channel.rs`), and the session ends when that
channel does. Inputs from a replaced session stop at once.
`claim_channel()` enforces one channel of each type per session: a new
link of the same type cancels the old claim, and `ChannelClaim`'s `Drop`
frees the type only if no later claim took it. `Cancel` is a one-way
signal on a `watch` channel that any number of tasks can await. The
session's clock (`started()`, `clock_ns()`) is the zero for pings and
for multimedia time.

### `andris/src/server/opcodes.rs`

One closed enum per channel of what andris may send, generated by the
`server_opcodes!` macro, with the largest client message each channel
accepts. This is how kerbside compatibility is enforced by construction:
a channel's connection can only send its own enum's variants, and the
test `opcodes_are_named_by_ryll` checks that ryll's
`logging::message_names` names every one. Read it before `channel.rs`,
which is generic over `ServerOpcode` and `AckedOpcode`. Note that
playback has no `SET_ACK` and so is not an `AckedOpcode`.

### `andris/src/server/channel.rs`

`ChannelConn<O>` owns a channel's socket: framing with ryll's
`make_message` and `ReceiveBuffer`, the messages every channel handles
itself (`PONG`, `ACK_SYNC`, `ACK`, `DISCONNECTING`), and the ack window.
`drive()` races a handler against session cancellation and records which
channel closed first, and why. Look at the cancel-safety claims, which
the display loop depends on: `recv()` and `wait_for_room()` keep
anything they have read, while `send()` is not cancel-safe and must run
in a `select!` arm's body. Look at `AckWindow`: the server may run up to
twice the window ahead, and `wait_for_room()` holds at most
`MAX_PENDING` client messages while it waits before giving up on a
client that withholds acks. Every write has `WRITE_TIMEOUT`. `close()`
shuts down andris's side and drains the client's, because a reset can
discard what was last sent. Ryll types: `messages::{AckSync,
Disconnecting, Pong, ReceiveBuffer, ReceivedMessage, SetAck, WireType,
make_message}`.

### `andris/src/server/main_channel.rs`

The main channel: `INIT`, the first `PING`, the channel list, mouse
mode, periodic pings and the metrics line. Look at `CHANNELS`: inputs
must be listed last because ryll stops reading the list at the inputs
channel. Look at `farewell()`: a replaced session's client gets a
`NOTIFY` naming who took over and a `DISCONNECTING`, and only then are
its other channels closed, so it hears why before it sees a channel go.
The server gives the goodbye `FAREWELL_TIMEOUT` before ending the
session regardless. The `select!` checks `is_replaced()` again in arms
that may run in the same poll as the replacement. Agent tokens go back
only when `has_unread()` is false. `SessionSummary` logs the session's
metrics from its `Drop`. Ryll types: `messages::{ChannelsList, MainInit,
MainMouseMode, MouseModeRequest, Notify, Ping, Disconnecting}`.

### `andris/src/server/stats.rs`

Per-session metrics as relaxed atomics, read as a `StatsSnapshot`.
Mostly bookkeeping. Look at how the compiler keeps it complete:
`encoding_slot()` matches every `Encoding` without a wildcard, and the
`since!` macro names every field, so a new counter fails to compile
until it is handled. `Reporter` produces the interval line that
`docs/running.md` explains field by field.

## Simple channels

Two channels that are small enough to read whole, and the scancode
decoder.

### `andris/src/server/inputs.rs`

Keys, lock keys, the pointer and buttons. Look at `HeldInput`: its
`Drop` releases every key and button still down through
`inject_release()`, which never blocks and has room even when the queue
is full, so a client that vanishes cannot leave a key held. When the
session is replaced the guard is dropped at once, not when the channel
closes, so the old client cannot lift a key the new one pressed. Look at
`set_locks()`: `KEY_MODIFIERS` is also ryll's idle keepalive, so only a
lock that differs is toggled, and never one whose key the client holds.
Absolute positions apply only in client mouse mode, relative motion only
in server mode, and every motion message counts towards
`MOUSE_MOTION_ACK` whether applied or not. The desktop size used for
clamping is cached for `GEOMETRY_MAX_AGE`. Ryll types:
`messages::{InputsInit, KeyEvent, KeyModifiers, KeyScancode,
MouseButton, MouseMotion, MousePosition}`,
`constants::{keyboard_modifiers, mouse_button_id}`.

### `andris/src/server/keymap.rs`

`KeyDecoder` turns set-1 bytes into evdev `KeyStroke`s, one decoder per
inputs channel so that an `E0` prefix ending one message applies to the
next. Look at the `E1` Pause sequence, which decoded naively would press
Ctrl and toggle Num Lock, and the "fake shift" bytes after `E0`, which
are dropped silently. Unmapped codes warn once per code.

### `andris/src/server/cursor.rs`

Each shape the platform reports becomes an `ALPHA` `CURSOR_SET`. Look at
the ack window: it waits for room before reading the latest shape, so
intermediate shapes are skipped rather than queued. The position sent is
always (0, 0), so a client in server mouse mode is never told where the
pointer is; `docs/running.md` says so. Shapes over `MAX_CURSOR_SIDE` are
dropped. Ryll types: `messages::{CursorHeader, CursorInit, CursorSet,
SpiceCursor}`, `constants::cursor_type`.

## The display channel

The largest part of the server. Read `display.rs` first for when things
happen, then the update pipeline for what is sent.

### `andris/src/server/display.rs`

The frame loop. Its module comment is the specification; read it first.
Look at the flow control in `run()`: a frame starts only when damage is
pending, the frame interval has passed, and the ack window has room.
Otherwise the loop waits for exactly the thing that would unblock it,
and never for damage it already knows is pending, which would spin.
Every branch of its `select!` must be cancel-safe, and `next_incoming()`
explains why waiting for room must read through `wait_for_room()`. Look
at failed captures: the damage is restored, all captures pause for the
longer of the frame interval and `CAPTURE_RETRY`, and the channel gives
up after `MAX_CAPTURE_FAILURES` in a row, or at once if the platform
says the desktop has gone. A capture of a new size replaces the surface,
ending every stream first. `with_encoder()` and `stream_frame()` move
the encoder, and the stream, into a blocking task and back; dropping the
future part way loses them, which happens only when the channel itself
is dropped. The scrub re-reads a band of the desktop each tick and adds
what damage missed. Ryll types: `messages::{DisplayInit,
DisplayMonitorsConfig, PreferredCompression, PreferredVideoCodecType,
StreamReport, SurfaceCreate, SurfaceDestroy}`, `capabilities`,
`constants::image_compression`.

### `andris/src/server/updates/mod.rs`

`UpdateEncoder`, the pipeline from a `Capture` to `Draw`s, and the
`Update` it returns. The module comment's five steps are the design.
Look at the shadow invariant: the shadow is brought up to date before
anything is encoded, and every rectangle is encoded from the shadow, so
a merged rectangle that covers unchanged or uncaptured tiles only
resends what the client already has. `update()`, `absorb()` (for a
stream's frame), `differences()` (for the scrub) and `refresh()` (for
lossless resends) are the four ways in. The first capture of a surface,
or of a new size, must be complete. Large rectangles are split by
`MAX_DRAW_BYTES` to stay far below kerbside's message cap.

### `andris/src/server/updates/shadow.rs`

`Shadow`, the client's pixels as andris last sent them, compared a tile
at a time with each patch of a capture. Look at partial tiles: a patch
that covers part of a tile is compared over that part alone. `apply()`
copies only the parts that differ. `image()` hands ryll's `Bgrx` a view
of a rectangle of the shadow.

### `andris/src/server/updates/encode.rs`

Choosing a draw's encoding and building its `DRAW_COPY`. Look at the
classifier constants: each says what measurements on real screens
showed, and `looks_photographic()` uses LZ4's own ratio as the signal.
JPEG is sent only if the client leaves compression to the server, the
rectangle looks photographic, and the JPEG is at most half of what
lossless would send. `split()` keeps pieces tile-aligned so no tile is
sent half in one draw. Ryll types: `encode_spice_jpeg`,
`encode_spice_lz4`, `Bgrx`, `messages::{DrawCopyBuilder, SpiceImage,
ImageDescriptor, BitmapHeader, ImagePayload, DrawBase, Clip}`,
`ImageType`.

### `andris/src/server/updates/lossy.rs`

`LossyTiles` tracks where the client holds only JPEG's approximation,
and when each tile falls due for a lossless resend. Each tile has its
own time, so an area that keeps changing does not hold back one that has
stopped. Look at the rule in `record()`: a lossless draw makes exact
only the tiles it covers wholly. `mark()` and `make_due()` are the
stream's hooks. `next_due` is kept current by every mutator, because the
display loop reads it on every wake.

### `andris/src/server/updates/heat.rs`

The heat map: per tile, the recent frames that changed it and whether
each went out photographic. A tile is hot after `HOT_FRAMES` changes
within `HOT_WINDOW` with no gap over `HOT_MAX_GAP`; candidates are
4-connected clusters of hot tiles at least two tiles each way, at least
`MIN_PHOTOGRAPHIC_PERCENT` photographic. Look at that last test: it is
what keeps a scrolling terminal from becoming a video, and it means a
client that turned JPEG off never gets a stream. `choose()` starts at
most one stream, and ends it when a touching cluster outgrows it by
`OUTGROWN_FACTOR`. Nothing here reads pixels.

### `andris/src/server/updates/stream.rs`

The one H.264 stream a display channel may have. Look at the stream
creation and teardown rules: `whole_tiles()` shrinks the area to whole,
even-sided tiles so no tile is part stream and part draws; ids cycle
through `STREAM_IDS` so a late report is not taken for the new stream;
and `Streams::take()` and `put_back()` move the live stream out for
encoding, with `put_back()` failing rather than overwriting another.
Look at `report()`: the "cannot decode" report counts with a `unique_id`
of 0, as spice-gtk sends it. `RateControl` cuts fast and raises slowly.
`pace()` keeps openh264's frame rate near the rate frames really come
at, because its budget per frame assumes the configured rate. Frames are
stamped `STREAM_LATENCY_BUDGET` ahead of capture. Ryll types:
`H264Encoder`, `H264Config`, `H264Content`, `H264Frame`,
`even_dimensions`, `target_bitrate_bps`, `messages::{StreamCreate,
StreamData, StreamActivateReport, StreamDestroy, StreamReport}`.

### `andris/src/server/updates/test_support.rs`

Test-only: a `Desktop` the update tests paint and capture, and a
`Client` that decodes each draw with ryll's readers and decoders and
checks every field andris fixes. Skim it before the update tests.

## Agent and sound

The main channel's guest agent, and the playback channel.

### `andris/src/server/agent.rs`

Andris plays the vdagent itself, since it runs inside the desktop it
exports. `Agent` handles `AGENT_START`, `AGENT_DATA` and `AGENT_TOKEN`
for the main channel. Look at the flow control both ways: the client's
window of `AGENT_WINDOW` chunks, with tokens handed back in batches and
only once everything read has been handled, and the outgoing queue
bounded by `MAX_QUEUED`, dropping rather than growing. Look at the
clipboard: text is fetched eagerly, compared with the last text that
crossed in either direction to break loops between two clipboard
bridges, and normalised to LF line ends before comparing because RDP
carries CRLF. The module comment names the known gap in that comparison.
A monitors config resizes through `DisplayControl` on the blocking pool,
and the main channel waits for the answer. Ryll types:
`messages::agent_stream::{AgentReassembler, AgentMessage,
AgentStreamItem}`, `messages::vd_agent::*`, `constants::vd_agent::*`,
`AgentTokens`.

### `andris/src/server/playback.rs`

`MODE`, then `START`, `DATA` and `STOP`, as Opus or raw PCM.
`SilenceGate` starts the stream on the first frame that is not digital
silence and stops it after `SILENCE_HOLD_FRAMES` silent ones. Look at
"Falling behind": there is no ack window and no timer; the sound server
paces the channel, and `limit_unsent()` sets `TCP_NOTSENT_LOWAT` so that
a stalled client makes capture's queue drop frames instead of the kernel
buffering seconds of late sound. Look at the two failure policies:
capture failing leaves the channel linked and silent, while the encoder
failing closes it. The Opus encoder is reset at each `START` to match
the client's fresh decoder. Ryll types: `link::SpiceStream`,
`messages::{PlaybackMode, PlaybackPacket, PlaybackStart,
MessageHeader}`, `audio_data_mode`, `audio_fmt`.

## The platform seam

The traits the server uses, and the types that cross them.

### `andris/src/platform/mod.rs`

The seven traits (`DisplayGeometry`, `ScreenSource`, `InputSink`,
`CursorSource`, `DisplayControl`, `Clipboard`, `AudioSource`), bundled
as `Platform`, and the plain types that cross them: `Size`, `Frame`,
`Patch`, `Capture`, `EvdevCode`, `InputEvent`, `CursorShape`. Look at
the "Threads" section: which methods may block and which never do. Look
at `Gone`, `is_gone()` and `GoneSignal`: an error that means the desktop
has gone for good, distinct from a capture that may succeed next time,
and the display and inputs channels decide whether to retry by it.
`InputSink`'s `inject_release()` exists so a release is never lost to a
full queue. `Size::check()` applies ryll's image limits, the only ryll
items this module uses.

### `andris/src/platform/damage.rs`

`Rectangle`, `Region` (a bitset of 64x64 tiles), and the `DamageHub` and
`DamageSubscription` pair. Look at "Taking damage": reporting and
`take()` use the same lock, and a consumer takes before it captures, so
an area may be captured twice but never missed. A new subscription
starts with the whole desktop pending, and `resize()` makes each
subscription's damage the whole new desktop. Look at
`Region::rectangles()`: four merge stages, each coarser than the last,
and the later ones cover tiles that did not change, which
`updates/mod.rs` makes harmless. `changed()` is cancel-safe by
`Notify`'s stored permit.

### `andris/src/platform/audio.rs`

`AudioFrame` (10 ms of 48 kHz stereo S16) and `audio_queue()`, a queue
of `AUDIO_QUEUE_FRAMES` that drops its oldest frame when full, so the
producer never waits. Dropping the receiver tells capture to stop. Look
at `MonotonicStamps`: a sound server's reported latency can rise faster
than time passes, so capture times are held from going backwards.

### `andris/src/platform/synthetic/mod.rs`

A deterministic desktop for tests, load testing and `--platform
synthetic`: colour bars, a moving square, paints, a panning "video", an
input recorder, an in-memory clipboard, and switches to make captures,
size reads or resizes fail. Look at how `paint_undamaged()`,
`report_damage()` and `set_size_unreported()` stage the faults the
display channel must survive, and at "Reporting changes": every change
is reported to the hub while the state lock is held, so damage and
resizes arrive in the order they were made. Captured rows carry padding
bytes so a stride bug shows.

### `andris/src/platform/synthetic/tone.rs`

`ToneSource`, a 440 Hz sine that sounds and falls silent in turn, paced
by tokio's clock so tests can step through it with the paused clock.
Each sample's angle is computed from its index in whole samples, so the
tone does not drift however long it plays.

## X11

The real backend: five connections to the X server, each on its own
thread.

### `andris/src/platform/x11/mod.rs`

`X11Platform::connect_with()` opens the connections (capture, input,
cursor, damage, clipboard), checks the server, and starts the threads.
The seam methods send requests down `std::sync::mpsc` queues and wait
for answers. Look at the X request stall handling in
`round_trip_warning()`: a capture or input request unanswered after
`STALL_WARNING` logs a warning, because the usual cause, another client
holding a server grab, is otherwise silent. The call keeps waiting, and
logs the answer when it comes. Look at how a dead connection becomes
`Gone` (`Fault`, `answer()`, `lost()`), and at shutdown: threads blocked
on X events are woken by a `ClientMessage` carrying a per-process random
cookie, because any client can find and message their windows. Note that
`platform()` leaves `audio` empty; `lib.rs` adds PulseAudio beside it.

### `andris/src/platform/x11/checks.rs`

`check()` decides from plain `ServerFacts` whether the display can be
exported: a 32-bit BGRX LSBFirst root window, and XTEST, XFIXES,
XKEYBOARD and DAMAGE. It names every problem at once. MIT-SHM and RandR
are optional; `resize_refusal()` gives the reason resizes will be
refused without RandR 1.2.

### `andris/src/platform/x11/capture.rs`

The capture thread: `GetGeometry`, then one image per rectangle by
MIT-SHM or `GetImage`, and RandR resizes. Look at the fallback ladder:
after `SHM_FAILURE_LIMIT` failed MIT-SHM captures it attaches a fresh
segment once, then gives MIT-SHM up for good, and the capture that hit
the limit is retried at once by the new means. `fit()` grows the segment
when the desktop grows. Look at the `gone` flag and the comment in
`serve_capture()`: the thread must stop on a lost connection, because
`shm.rs` may return with requests still in flight. Resizes run on this
thread, so a resize holds up captures while it lasts. Read before
`shm.rs`, whose soundness argument relies on this thread's behaviour.

### `andris/src/platform/x11/shm.rs`

MIT-SHM capture through a sealed memfd passed with `ShmAttachFd`, and
one of the crate's three `unsafe` modules. Look at the trust boundary in
the module comment: any client of the display, or any process that can
open the memfd, may write the segment at any time. So andris never makes
a Rust reference into the mapping; `Mapping::copy()` copies out through
raw pointers, so a concurrent writer can only tear pixels. The seals
stop the file shrinking, so reading cannot fault. `Segment::capture()`
waits for every reply in a batch before copying, even after a failure.
`is_unix_socket()` matters: x11rb may have fallen back to TCP, which
cannot pass a descriptor. Read every `SAFETY:` comment against the code
beneath it.

### `andris/src/platform/x11/damage.rs`

The damage thread: a `RawRectangles` damage object on the root window
and the root window's `ConfigureNotify`, batched into the hub, at most
`BATCH_LIMIT` events at a time so a constant stream still reaches it. A
resize replaces the batch's damage with the whole desktop. Look at
`start_damage()`: the size is read after asking for size changes, so a
resize is either seen there or arrives as an event. If the thread stops
for any reason but shutdown, it marks the hub gone, so no subscriber
waits for damage that will never come.

### `andris/src/platform/x11/cursor.rs`

The cursor thread: XFixes cursor notifications, each followed by
`GetCursorImage` and published on a `watch` channel. Short.

### `andris/src/platform/x11/input.rs`

The input thread: XTEST injection and the lock state from XKB
indicators, both through one FIFO. Look at `InputQueue`: one unbounded
channel, bounded by a counter, so that `inject()` and `inject_release()`
differ only in their limit and a release can never overtake its press. A
lock-state read queued behind events is answered after them, which is
what `ServerHandle::stop()` relies on. Events are flushed in batches.
Lock indicators are found by name, and one the server lacks always reads
as off.

### `andris/src/platform/x11/keymap.rs`

Evdev code to XKB key name through the generated `EVDEV_NAMES` table
(from `scripts/gen-evdev-names.py`), then name to keycode through the
names the server reports with XKB `GetNames`, aliases included. This is
what makes keys land whichever keycode set the server uses. The names
are read once, at startup, so a later `setxkbmap` is not seen.

### `andris/src/platform/x11/randr.rs`

Resizing through RandR 1.2 on the capture connection. The module
comment's six steps are the algorithm. Look at `plan()`: the screen and
CRTC change in an order that keeps the CRTC inside the screen at every
step. Look at the read-back: a server can answer Success and change
nothing (xorgxrdp does), so the result is read again, and on any failure
the steps are undone in reverse and the modes added are removed.
Clamping to the server's range counts as applied, unless it leaves the
desktop where it was, which is a refusal. Modes named `andris-` are
treated as andris's own, so two andris processes on one display may
remove each other's unused modes.

### `andris/src/platform/x11/clipboard.rs`

The clipboard thread, the largest X11 file: following `CLIPBOARD`'s
owner with XFixes and fetching its text, and owning the selection to
serve andris's text, INCR transfers both ways. Look at "The event loop":
the thread never waits on another client, keeps a deadline on every
conversion and transfer, and a separate timer thread wakes it when one
passes. Look at `acquire()`: the selection is taken with a server
timestamp got from a zero-length property append, and checked
afterwards. Andris's own text is never published back; taking the
selection publishes `None`. Text either way is capped at
`MAX_CLIPBOARD_TEXT`, and at most `MAX_TRANSFERS` large transfers out
run at once.

## PulseAudio

Sound capture, and another of the crate's `unsafe` modules.

### `andris/src/platform/pulse/mod.rs`

`PulseSource`, the `AudioSource` for `--platform x11`: one thread per
capture, reading exactly one 10 ms frame per `pa_simple_read`, with a
one-frame fragment size so frames arrive as captured. Look at
"Stopping": a read can block for seconds while a sink wakes, so the
thread is never joined and libpulse is never unloaded, and
`MAX_LIVE_CAPTURES` stops a hung sound server collecting a thread per
relink. Read this before `ffi.rs`, whose safety comments rely on that
policy. `load()` caches the first answer for the life of the process.

### `andris/src/platform/pulse/ffi.rs`

Hand-written bindings to five libpulse functions, loaded with `dlopen`
so the binary links no libpulse and runs without it. Look at every
constant, `#[repr(C)]` struct and function type against the header it
cites; the tests check them against `libpulse-sys`. Look at
`Api::open()`: `RTLD_NOW` so a broken library fails at load rather than
mid-capture, and symbols looked up as `Option<fn>` so a null is not an
invalid function pointer. `Simple` owns one connection, frees it exactly
once, and is neither `Send` nor `Sync`.

## The qemu platform

Built only with the `qemu` cargo feature. Read `mod.rs` first.

### `andris/src/platform/qemu/mod.rs`

`QemuPlatform`: attaching (QMP, then the display connection, then the
listener's), and the seams over what the listener keeps. Look at
`attach()`: qemu answers `RegisterListener` before it opens the
listener's connection, and both listener interfaces must be served from
the start, since qemu asks for the shared screen at once. Look at
`start()`: QMP is closed once qemu has the display socket, and qemu's
exit is seen when it closes the display connection, which the platform's
thread waits for; that thread lets the connections go if attaching
fails or times out, so qemu drops the client.

### `andris/src/platform/qemu/qmp.rs`

Just enough QMP to pass qemu a socket (`getfd` with `SCM_RIGHTS`, then
`add_client`); andris closes it straight after. Lines are bounded, and
every read and write times out.

### `andris/src/platform/qemu/listener.rs`

The objects qemu calls with each change: pixels in the message, or a
shared memfd and then bare rectangles, plus the cursor. Look at
`scanout_map()`, which fails back to qemu for a screen andris cannot
map or refuses, so that qemu sends pixels in the message instead, and
keeps no mapping; `update_map()`, which copies only the rectangle's
span of each row out of the mapping and converts from that copy; and
`set_cursor()`, which reports only a change of what is shown, since
`MouseSet` comes with every pointer move.

### `andris/src/platform/qemu/shm.rs`

The mapping of the screen qemu shares, and the third of the crate's
`unsafe` modules. Look at the trust boundary in the module comment: qemu
and the guest write the screen while andris reads it. So, as in the X11
platform's `shm.rs`, andris never makes a Rust reference into the
mapping; `Mapping::rows()` copies out through raw pointers, so a
concurrent writer can only tear pixels. `Mapping::new()` refuses a memfd
shorter than the screen it claims to hold, since reading past the end
of a mapping faults.

### `andris/src/platform/qemu/framebuffer.rs`

The platform's own copy of the screen, and pixman format conversion.
Every copy is bounds-checked against what qemu sent or andris copied
out of the mapping, and a
refused replacement leaves the screen as it was.

### `andris/src/platform/qemu/input.rs`

Input to qemu's keyboard and mouse, on a counted FIFO like the X11
platform's. Look at `Pointer`, which turns positions into offsets and
back for whichever mouse the guest has. `qnums.rs` is generated by
`scripts/gen-qemu-qnums.py`.

### `andris/src/platform/qemu/tests.rs`

One test that plays qemu over a peer-to-peer D-Bus connection: shared
screen, updates, keys, mouse, lock state and cursor.

## Tests

Unit tests live beside the code. These are the shared helpers and the
integration tests in `andris/tests/`, most of which run an in-process
andris on the synthetic platform and talk to it as a SPICE client built
on ryll's crates. Every wait has a timeout.

### `andris/src/test_logs.rs`

`Logs`, a `tracing` writer that collects one thread's log lines for a
unit test to check. Short.

### `andris/tests/support/mod.rs`

The harness every integration test uses: `TestServer` (an in-process
andris on the synthetic platform, with a random ticket), `TestClient`
(ryll's client-side link drivers), and timed waits such as
`wait_until()` and `closed_within()`. Read it before any test file.

### `andris/tests/support/display.rs`

The display channel's client side, shared by `display.rs` and
`stream.rs`: the start sequence, and a `Screen` that applies decoded
draws so that it can be compared with the synthetic desktop.

### `andris/tests/link.rs`

Linking, authentication, sessions and listeners: refusals, the handshake
limits and time limits, TLS and the redirect to it, and a second link
replacing the first.

### `andris/tests/main_channel.rs`

The main channel's opening messages, the channel list, mouse mode, and
what a replaced client is told.

### `andris/tests/inputs.rs`

Keys, lock keys, the pointer, buttons, motion acks, and releasing what
is held. Note the sentinel key (F12) a test sends to know the server has
read everything before it.

### `andris/tests/cursor.rs`

`SET_ACK` and `CURSOR_INIT`, a `CURSOR_SET` per shape change, the size
limit and the ack window.

### `andris/tests/display.rs`

Frames, compression choice, the rate cap, the ack window, the scrub,
resizing, failed captures, and JPEG with its lossless refresh. Every
draw is decoded and the client's surface compared with the desktop.

### `andris/tests/stream.rs`

Video streams: when one starts, what it sends, how it ends and its area
is repaired, reports and rate control. Frames are decoded with ryll's
H.264 decoder.

### `andris/tests/agent.rs`

The guest agent: flow control, capabilities, resizing and the clipboard
both ways.

### `andris/tests/playback.rs`

The playback channel: mode choice, the silence gate, multimedia times,
drops under a stalled reader, `--no-audio` and capture failing. Opus is
decoded with ryll's decoder.

### `andris/tests/lifetime.rs`

`serve_until()`: a stand-in signal stops andris cleanly, and the desktop
going stops it with an error. No real signal is sent.

### `andris/tests/cli.rs`

Runs the built binary and checks what it prints and how it exits.

### `andris/tests/x11.rs`

The X11 backend against a real X server named by `ANDRIS_TEST_DISPLAY`,
which `make e2e` sets: damage, capture by both means, recovery after
another client detaches the MIT-SHM segment, RandR, and the clipboard.
Without the variable it skips and passes.

### `andris/tests/pulse.rs`

PulseAudio capture against a real sound server, only when
`ANDRIS_TEST_PULSE` is set; otherwise it skips and passes. With it set
and `PULSE_SERVER` unset it fails, rather than play a tone into the
desktop's own sound server.

### `andris/tests/jpeg_corpus.rs`

Not a test of behaviour, and `#[ignore]`d: the measurement behind the
JPEG classifier's constants in `updates/encode.rs`, run over a corpus of
screen captures that is never committed.

## Build and packaging

How the code is built, packaged, pinned and checked.

### `Cargo.toml`

The workspace: one member, edition 2024, the `unwrap_used` lint, and the
shared version that releases bump. `profile.dev` keeps line tables only,
to keep `target/` small.

### `andris/Cargo.toml`

The crate's dependencies, each with a comment saying why it is there and
which version it shares with the tree. Look at the ryll pin: both
`shakenfist-spice-*` crates at one git revision, which must move
together, and in `[dev-dependencies]` too, where extra features add the
decoders tests need.

### `andris/pyproject.toml`

The maturin manifest for the PyPI wheel, `bindings = "bin"`, so the
wheel carries the compiled binary. Its comment explains why the
manylinux tag is earned rather than asserted: libpulse is loaded, not
linked.

### `Makefile`

Every cargo invocation runs in the `andris-dev` container. Look at the
security model in its comments: compiling targets run with `--network
none`, a read-only checkout with only `target/` writable, and a
read-only crate cache, so a dependency's build script can neither phone
home nor poison the cache or `.git`. The targets with the network
compile none of andris, as `make fetch` and `make kerbside-demo` do, or,
like `make ryll-headless`, run in a sandbox of their own. `make
lint-fix` is the exception, with a writable checkout, and says so. The
ryll revision for `make ryll-headless` is read out of `Cargo.lock`.

### `packaging/systemd/andris@.service`

The user unit, an instance per display number. Look at
`Restart=on-failure` with `RestartPreventExitStatus=2`, which matches
`main.rs`'s exit statuses, and the hardening settings, among them a
system call filter. `install.rs` embeds this file and rewrites its
`ExecStart=`.

### `deny.toml`

cargo-deny's policy: the licence allowlist, the advisories ignored, each
with a rationale and URL, and the target triples checked.

### `.cargo/audit.toml`

cargo-audit's ignore list, which must mirror `deny.toml`'s; the
rationale lives there.

### `clippy.toml`

One line: unwraps are allowed in tests.

### `renovate.json`

Renovate's configuration: nothing merges automatically, pre-commit hooks
are watched, and a note for Renovate's own major bumps.

### `.gitignore`

Build output, the in-checkout cargo cache, editor, OS and Python cache
files. `.vscode/` is ignored as `.vscode/*` with exceptions, so that
the weAudit review marks, their SHA sidecars and `review-scope.toml`
can be committed while the weAudit day log and editor settings stay
ignored. What this file hides is a security property: an ignored file
is never reviewed and never scanned.

### `.pre-commit-config.yaml`

The hooks: rustfmt and clippy through `make lint`, the release version
script's fixtures, actionlint, the usual whitespace and YAML checks,
shellcheck, gitleaks and skillsaw. Note that the Rust hook runs Docker.
The two rewriting hooks, trailing-whitespace and end-of-file-fixer,
skip the weAudit marks, which a rewrite would corrupt. That exclusion
is per hook, not top-level, so that gitleaks still reads review notes.

### `.devcontainer/Dockerfile`

The `andris-dev` image: the build tools, Xvfb and the X clients, and
PulseAudio, for the tests and `make e2e`. Note that it installs Rust
stable unpinned, unlike the wheel image.

### `.devcontainer/devcontainer.json`

The VS Code devcontainer definition for the same image.

### `.devcontainer/wheel/Dockerfile`

The wheel build image on the manylinux_2_28 base, pinned by digest, with
Rust and maturin at exact versions, since the wheel is a published
artifact.

### `.devcontainer/wheel/requirements.txt`

maturin, pinned with every hash PyPI publishes for it.

### `.devcontainer/wheel-smoke/Dockerfile`

A stock `python:3-slim`, with no libpulse, X server or systemd, for
installing the wheel as a bare host would.

## Scripts

`scripts/` holds what `make` runs beyond cargo. `e2e.sh` is the
end-to-end test against Xvfb, driven through ryll's control socket by
`e2e-drive.py`, with `e2e-keylogger.py` as the window keys are typed
into and `e2e-video.py` as the moving picture that should become a
stream. `e2e-kerbside.sh`, with `kerbside-lane/`, runs the same checks
through kerbside's demo stack; `install-compose.sh` gives a CI runner
`docker compose` for it. `e2e-xrdp.sh`, with `xrdp-testbed/` and
`e2e-xrdp-drive.py` and `e2e-pasteboard.py`, shares a real xrdp session
in a throwaway container. `build-andris-wheel.sh` and its in-container
half build the manylinux wheel, and `wheel-smoke.sh` installs it on a
bare image. `release-checksums.sh` checks a release's wheels.
`check-rust.sh` is the pre-commit hook. `gen-evdev-names.py` generates
`platform/x11/evdev_names.rs` and its test fixtures, and
`gen-qemu-qnums.py` generates `platform/qemu/qnums.rs`; read the
generators rather than their output.

`tools/` holds the release scripts, copies of shakenfist/development's
Rust release template, apart from their marked settings:
`propose-release.sh` and `tag-release.sh` for the two stages,
`release-version.sh` and its test, and `sign-release-tag.sh`, run by the
release workflow. `docs/releasing.md` describes how they fit.
`review-tracking.sh` and `ci-prune-reviews.sh` are the review-tracking
template's copies: the first is the wrapper behind `next` and `status`,
and the second is what the prune bot runs.

## CI

`.github/workflows/ci.yml` is the two-tier pipeline: a smoke tier on
pull requests gating "Can enqueue", and a merge tier on the merge group
gating "Can merge", with gate jobs whose `jq` treats a skipped job as
passing. Read its header comment, which explains why path filtering is
done per job rather than per trigger. `release.yml` builds, signs and
publishes a release from a tag. `secret-scan.yml` reads everything,
documentation included; `codeql-analysis.yml`, `supply-chain.yml` and
`renovate.yml` are the scanners and the dependency bot; `pr-retest.yml`
and `pr-re-review.yml` answer bot commands in comments;
`export-repo-config.yml` snapshots the repository's GitHub settings into
`.github/exported-config/`, which is generated; `prune-reviews.yml`
keeps the review marks honest, dropping any whose file has changed.
`actions/cargo-cache/` is the cache step every devcontainer job shares,
and `actionlint.yaml` declares the runner labels. Several of these are
copies of shakenfist/development's templates, and say so in their
headers.

## The rest

`docs/development.md` is the developer's manual: make targets, the
end-to-end lanes, why compiling is offline, pre-commit and CI.
`docs/releasing.md` is the release process and its setup. `AGENTS.md`
holds the conventions for AI coding assistants, many of which this guide
has met in the code: the opcode enums, evdev codes at the seam, the
`unsafe` modules. `PUSH-AUDIT.md` is the pre-push audit runbook, and the
planning template beside it at the repository's root is where new plans
start. These are process documents, so read them for how work is done
here rather than for how andris works.
