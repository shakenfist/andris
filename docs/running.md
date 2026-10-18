# Running andris

How to build andris, point it at an X display, and connect to it with
ryll, directly or through kerbside. For building and testing the
code itself, see [development.md](development.md).

Andris exports a display that is already running. It never starts or
configures a desktop: it connects to the X server as an ordinary
client, captures the root window, and injects keys and pointer events
with the XTEST extension.

## Building

```bash
make release
```

builds `target/release/andris` inside the devcontainer; `make build`
makes a debug binary at `target/debug/andris`. See
[development.md](development.md) for why the build runs in Docker.
The binary is an ordinary dynamically linked executable, so copy it
to the machine whose desktop you want to export. Besides the C
library it needs only libpulse at run time (`libpulse0` on Debian).

Ryll's `shakenfist-spice-compression` crate bundles openh264, so the
first build also compiles it even though andris only encodes images
([ryll#499](https://github.com/shakenfist/ryll/issues/499)).

## Options

```text
andris --ticket-file PATH [options]
```

| Option | Default | Meaning |
|--------|---------|---------|
| `--ticket-file PATH` | none; required | File holding the SPICE ticket (password) clients must present |
| `--platform x11\|synthetic` | `x11` | The display system to export |
| `--display DISPLAY` | the `DISPLAY` environment variable | The X display to export, such as `:10`; used by `--platform x11` |
| `--synthetic-size WxH` | `1024x768` | The test pattern's size; used by `--platform synthetic` |
| `--listen ADDR:PORT` | `127.0.0.1:5900` | The plain TCP listener |
| `--tls-listen ADDR:PORT` | none | A TLS listener; needs `--tls-cert` and `--tls-key` |
| `--tls-cert PATH` | none | The TLS certificate chain, as PEM, leaf first |
| `--tls-key PATH` | none | The TLS private key, as PEM |
| `--frame-interval-ms MS` | `33` | The shortest time between two frames, a cap on the frame rate (33 is about 30 frames a second); at least 20. It is a cap, not a timer: see [What a client gets](#what-a-client-gets) |
| `--jpeg-quality QUALITY` | `85` | The JPEG quality, 1 to 100, for areas that look photographic; `0` never sends JPEG (see [What a client gets](#what-a-client-gets)) |
| `--no-audio` | off | Capture no sound, and offer clients no playback channel (see [Audio](#audio)) |
| `--audio-source NAME` | `@DEFAULT_MONITOR@` | The PulseAudio source to capture the desktop's sound from; used by `--platform x11`, and refused with `--no-audio` and with `--platform synthetic` |

`andris --help` prints the same list. Logs go to stderr at `info`
level; set `RUST_LOG` (for example `RUST_LOG=andris=debug`) to
change that. SIGINT (Ctrl-C) and SIGTERM (what systemd and `kill`
send) each shut andris down cleanly. It closes the client's
connections, releases any key or mouse button the client still held,
and waits up to five seconds for the desktop to take those releases
before it exits with status 0, so nothing is left held down on the
desktop.

Andris attaches to a desktop; it does not own one. If the X server it
exports goes away (the session logs out, or the server exits or is
killed), andris logs an error saying why, closes the client's
connections within a second, and exits with status 1. There is no
desktop left to release held keys into. It does not wait for a new X
server; whatever started andris must start it again once there is one.

Options are checked together before anything starts, and each error
names the option at fault and how to fix it. The three TLS options go
together: give all or none.

So andris exits with one of three statuses:

| Status | Meaning |
|--------|---------|
| 0 | Stopped by SIGINT or SIGTERM |
| 1 | The desktop went away, or another failure after the options were accepted, such as an X server that cannot be reached or a listener that cannot bind |
| 2 | The options were refused (a bad combination, a ticket file that is unreadable, not private, empty or too long, or a TLS certificate or key that cannot be loaded), and nothing was started |

Status 2 means a person has to change something before andris can
start; the systemd unit in [xrdp.md](xrdp.md) does not restart after
it.

## Exporting an X display

```bash
(umask 077; head -c 24 /dev/urandom | base64 > ticket)
DISPLAY=:10 XAUTHORITY=$HOME/.Xauthority \
    target/release/andris --ticket-file ticket
```

Andris must run as a user the X server will accept, which for an
xrdp session is the session's user. The display comes from
`--display`, or failing that `DISPLAY`; with neither, andris refuses
to start. Authorisation uses the usual X mechanisms, so set
`XAUTHORITY` if the session's cookie is not in `~/.Xauthority`.

To run andris as a service beside xrdp, see [xrdp.md](xrdp.md).

Before it listens, andris checks the X server and refuses to start,
naming every problem it found, unless:

- the root window's depth is 24 or 32, stored at 32 bits a pixel;
- the image byte order is LSBFirst and the root visual is TrueColor
  with the usual BGRX masks (`0xff0000`, `0x00ff00`, `0x0000ff`);
- the XTEST, XFIXES, XKEYBOARD and DAMAGE extensions are present.

Xvfb at 24 bits meets these, and the end-to-end test runs against
it.

The DAMAGE extension is how andris learns which parts of the desktop
changed, so an X server without it is refused. MIT-SHM is optional.
Andris reads the screen with it when it can: it shares a memfd with
the X server (`ShmAttachFd`, MIT-SHM 1.2), so pixels do not cross the
X socket. It falls back to the slower `GetImage` when the server lacks
MIT-SHM 1.2, when the display is reached over TCP (a file descriptor
cannot cross it), or when the server refuses the shared segment. Which
it uses, and why, is logged at info at startup, as
`capturing the screen with MIT-SHM, through a shared memfd` or
`capturing the screen with GetImage, because ...` followed by the
reason. Use a local display (`:10`, not `host:10`) to get MIT-SHM.
If captures through the shared segment keep failing, as they do if
another client detaches it, andris attaches a fresh one once and then
switches to `GetImage` for good, logging a warning each time.

The server's keyboard layout is read once at startup, so a layout
changed afterwards with `setxkbmap` is not noticed until andris
restarts.

## The synthetic platform

```bash
target/release/andris --platform synthetic --ticket-file ticket
```

exports a generated test pattern instead of a display: eight colour
bars with an orange square that moves along them. It needs no X
server and records the input it is given rather than injecting it.
It is for trying a client, load testing, and the tests.
`--synthetic-size` sets its size.

## The ticket file

Clients authenticate with a SPICE ticket, which is a password
andris reads from `--ticket-file`. The file must:

- be a regular file owned by the user andris runs as, whose mode
  gives group and other no access, so `chmod 600` it; andris refuses
  a file with any group or other bit set, or owned by anyone else;
- hold 1 to 60 bytes (SPICE's limit), not counting one trailing
  newline, which is stripped.

Make the ticket long and random:

```bash
(umask 077; head -c 24 /dev/urandom | base64 > ticket)
```

writes 32 characters of base64 to a file only you can read. Length
matters because andris only slows wrong guesses, and never locks a
client out: a client whose ticket is wrong waits a second for its
answer, and each address may have at most four handshakes in
progress. A short ticket can still be guessed, so andris logs a
warning at startup for one under 16 bytes, but accepts it.

The ticket is read once at startup, and is never logged. Each wrong
ticket is logged at `info` with the client's address, for tools such
as fail2ban, at most once every 10 seconds with a count of those
since the last.

The `--tls-key` file is held to the same rules of ownership and mode.

## Listeners, TLS and the loopback rule

By default andris listens for plain TCP on `127.0.0.1:5900`. Because
SPICE sends the ticket over the connection, andris refuses to start
with a `--listen` address that is not a loopback address (`127.0.0.0/8`
or `::1`) unless a TLS listener is also configured.

To serve beyond the local machine, add a TLS listener:

```bash
target/release/andris --ticket-file ticket \
    --listen 127.0.0.1:5900 \
    --tls-listen 0.0.0.0:5901 --tls-cert cert.pem --tls-key key.pem
```

With TLS configured, the plain listener stops serving sessions: it
answers every link with "need secured", which sends a SPICE client
to the TLS listener. With TLS configured, `--listen` may be any
address. Clients are not asked for certificates.

Unauthenticated connections are limited, so that no one peer can
lock others out:

- at most 8 connections may be in their handshake at once on the
  listeners that serve SPICE;
- with TLS configured, the plain listener's redirects have their own
  limit of 4, so a flood of the plain port cannot starve the TLS one;
- at most 4 connections from one address may be in their handshake
  at once, on any listener, so that no one address can hold all 8;
  four lets a client link its main channel and then its display,
  cursor and inputs channels in parallel, as spice-gtk does. An IPv6
  address counts by its /64;
- at most 2 RSA keys for ticket encryption are generated at once.

Connections past a limit are closed immediately, and the refusals
are logged at most every 10 seconds. The first step of a handshake,
which a peer can stall without having proved anything, has a 3
second limit: on the TLS listener the TLS handshake and then the
SPICE link message each get 3 seconds, and on the plain listener the
link message does. Every later step has a 10 second limit.

Andris serves one client at a time. A client that authenticates
replaces the one before it, whose channels are closed. A session
has at most one channel of each type: a new link of a type the
session already has replaces, and closes, the old channel.

## What a client gets

A connection opens five channels, and ryll opens all five:

| Channel | What andris sends or does |
|---------|---------------------------|
| main | Opens the session, lists the other channels, applies the client's mouse mode request, pings every 30 seconds, and plays the guest agent (below), which carries the clipboard and desktop resizing |
| display | The desktop as images (below) |
| inputs | Applies the client's keys and pointer events to the desktop, and reports the desktop's lock-key state |
| cursor | The pointer's shape as it changes, as an alpha cursor |
| playback | The desktop's sound, as Opus or raw PCM (see [Audio](#audio)); absent with `--no-audio` |

**Display.** Andris sends the first frame whole. After that it
captures only the parts of the desktop the platform reports as
damaged, compares them, in tiles of 64x64 pixels, with what it last
sent, and sends only the tiles whose pixels changed, merged into at
most 32 rectangles a frame and followed by a `MARK`, which ryll counts
as a frame. A desktop that does not change captures and sends no
frames. Frames start at most once every `--frame-interval-ms`, and
only when there is damage to send, so the option bounds the frame rate
and the work a busy desktop costs; it does nothing for a still one.
Each rectangle is a JPEG
when it looks photographic and JPEG is allowed (below), an
LZ4-compressed image when the client can decode LZ4 and the result is
smaller, and an uncompressed bitmap otherwise; a client that asks for
no compression gets bitmaps.
If the desktop changes size, the display surface is recreated at the
new size. A slow client is not flooded: while too much is
unacknowledged, no frame starts, and changes merge until the client
catches up, when one frame brings it up to date. A capture that fails,
as one can while the desktop is being resized, is retried and logged
as a warning; the display channel closes after ten failed captures in a row,
frame or scrub, or at once if the connection to the X display has gone.

**The scrub.** The X server does not report every change, so andris
also re-reads the whole desktop at least once every four seconds, one
slice of whole tile rows every 250 ms. It compares each slice with
what it last sent, and any tile that differs is sent in the next frame as if it had
been reported. A change damage missed therefore reaches the client
within about four seconds. The scrub runs even on a still desktop,
where it sends nothing but still costs a slice's capture and
comparison each tick (see [High CPU use](#high-cpu-use)). It
pauses while the client is behind (the ack window is full) and for a
moment after a failed capture.

**JPEG.** Andris sends JPEG, at `--jpeg-quality`,
only when the client leaves the choice of compression to the server:
it has sent no compression preference, or has asked for `auto-glz` (ryll's default) or
`auto-lz`. A client that asks for a particular compression (`lz4`,
say) gets lossless images only, and one that asks for `OFF` gets bitmaps, and `--jpeg-quality 0` turns JPEG off
for every client. A rectangle goes as JPEG when it covers at least 4096
pixels and its shortest side is at least 16 pixels, LZ4 would save less than half of its bytes, and the JPEG
is at most half the size of what would otherwise be sent. Text and
user interface compress far better than that, so they are never sent
as JPEG; photographs are. Smooth gradients compress well
enough with LZ4 that they stay lossless.

A JPEG is approximate, so the tiles it drew are sent again
losslessly once they have gone half a second without changing. That
refresh is a frame of its own, followed by a `MARK`, and waits for the
frame rate cap and for a slow client like any frame. A still screen
therefore always ends up exact, while an area that keeps changing,
such as a video, stays JPEG for as long as it changes, unless the
client can take it as a [video stream](#video-streams).

**Keys.** Keys are typed by position, not by character. The client's
scancodes become evdev key codes, and those become whichever
keycodes this X server uses for the same keys, so they work whatever
keycodes the server was configured with.

**Mouse modes.** The session starts in client mouse mode: the client
sends absolute positions and draws the pointer itself. A client may
ask for server mode, in which it sends relative motion. Andris applies
absolute positions only in client mode and relative motion only in
server mode.

Server mode moves the desktop's pointer, but andris does not yet tell
the client where that pointer is: it sends no `CURSOR_MOVE`, and every
cursor shape carries position (0, 0). A client in server mode that
draws the pointer from the server's reports, as spice-gtk does, draws
it in the wrong place. Use client mode, which ryll uses.

**The agent.** A SPICE server usually relays the main channel's agent
messages to a helper program running in the guest. Andris runs inside
the desktop it exports, so it plays that agent itself: the agent is
connected from the moment the main channel opens, and stays connected
until the channel closes. There is nothing to install or start in the
desktop. The agent carries the clipboard and desktop resizing, and is
what makes a client's "share clipboard" and "resize guest to window"
options work. A client must reassemble agent messages that span several
chunks, which ryll does only from a revision with agent reassembly, so
use ryll from `develop` as described in
[Connecting with ryll](#connecting-with-ryll).

**Copy and paste.** The desktop's `CLIPBOARD` selection and the
client's clipboard are shared in both directions, as UTF-8 text. When
the client copies, andris fetches the text at once and puts it on the
desktop's clipboard; when text is copied on the desktop, andris offers
it to the client, and sends it when the client asks. Text over 16 MiB
is dropped, with a warning in the log, and the clipboard keeps what it
held. Only text on `CLIPBOARD` crosses: the `PRIMARY` selection (select
to copy, middle-click to paste) and images are not carried, and a
client's request for anything else is answered with an empty clipboard.
Text identical to the text that last crossed, in either direction, is
not sent again; this stops two clipboard bridges on one machine
passing a copy back and forth for ever (see
[xrdp.md](xrdp.md#clipboard-with-rdp-and-spice-together)). If the
desktop's clipboard owner does not answer a request for its text within
10 seconds, andris gives up on that fetch and offers the client
nothing.

**What the clipboard shares.** Because andris offers each copy as it
is made, and ryll fetches what it is offered at once, every text copied
on the desktop reaches the connected SPICE client whether or not anyone
pastes it there: text copied from a password manager included. A
password manager that clears the clipboard later withdraws the offer,
but the client already has the text. Connect only clients you would
trust with the session's clipboard. Andris offers nothing to a client
that does not announce clipboard sharing.

**Resizing.** When the client asks for a desktop size, as ryll does
when its window is resized, andris changes the X server's size through
RandR, and the display surface follows (see Display above). A size
outside what the X server allows is clamped to its range: the client
is told the size it actually got, or, if the desktop already has the
clamped size, that the request was refused. The resize needs an X
server whose RandR allows a client to change the screen size. Xvfb
does, up to the size it started with. xorgxrdp does not (see
[xrdp.md](xrdp.md#resizing)). Where it is refused, the client gets an
error reply, the desktop keeps its size, and andris logs one warning
for the connection, not one for each request. RandR is optional:
without it, or without RandR 1.2, andris still serves the desktop and
refuses every resize in the same way.

### Video streams

An area that keeps changing like a video can go to the client as an
H.264 stream rather than as a JPEG for every frame. Andris starts one
when a region of the desktop has changed in at least 20 frames within
two seconds, with no gap over 200 ms between changes, and at least a
fifth of those changes went out as JPEG, so it looks photographic.
The region must be at least 128 pixels each way, and at most 3840 by
2160, either way round, which is the most the encoder takes; a larger
one stays on JPEG draws whole, rather than being streamed in part.
A stream is made of whole 64-pixel tiles with even sides, so where a
video reaches an edge of the desktop whose size is odd, the last few
pixels along that edge go as draws.
Text, a scrolling terminal or a progress bar changes as often but
compresses well, goes as LZ4, and never streams; neither does anything
when JPEG is off (see above), since a client that asked for exact
pixels gets them.
There is at most one stream on a connection, the largest such region.

Inside the stream's rectangle, each frame that changes any of it goes
out as one stream frame of the whole rectangle, in place of draws.
Everything else on the desktop is drawn as usual. A stream ends after
a second without a change in its rectangle, or when the changing area
grows to more than twice its size (a video window enlarged or moved),
and detection then starts afresh. A resize of the desktop ends it too.
When it ends, the stream's area is sent again losslessly at once, so
that text near a paused video, or the paused picture, is exact in
about a second.

A client gets streams only if it advertises both the `MULTI_CODEC` and
`CODEC_H264` display capabilities; andris encodes only H.264, so the
client's codec preference order changes nothing. Any other client gets
JPEG draws, as above. Ryll advertises H.264 after MJPEG, and
`--no-h264` turns that off; a ryll older than that change
([ryll#552](https://github.com/shakenfist/ryll/pull/552)) gets JPEG
draws. A client that reports it cannot decode a stream, or an encoder
failure, ends the stream and stops any more on that connection, with
one warning.

Andris tunes the encoder for video and gives it a bitrate budget for
the stream's size and frame rate. It asks the client for a report
every five frames, and lowers the budget quickly when the client says
frames are late or arriving with little time to spare, or when the ack
window is full (a server drop) twice in a second. It raises it by a
tenth after two seconds of clean reports, never above its starting
budget or below 256 kbit/s. It also keeps the encoder's frame-rate
budget near the rate it really sends at, since a stream sends frames
only when its area changes. When the encoder skips a frame to hold the
budget, nothing is sent for it. A client without the `STREAM_REPORT`
display capability still gets streams, but is never asked for reports,
so only server drops lower its budget, and nothing raises it.

Each frame is timestamped 80 ms after it was captured, which is the
latency budget: a client counts a frame late only when sending it took
longer. A client that presents frames at their timestamp, such as
spice-gtk and remote-viewer, therefore shows a stream up to 80 ms
behind the desktop. Ryll paints frames as they arrive and pays
nothing.

Andris logs at info when a stream starts, with its rectangle and the
bitrate it aims for, and when it ends, with why:

```
starting a video stream session=1 stream=0 rect=320x256+64+64 target_kbps=1000
ending a video stream session=1 stream=0 why=it has been still
```

The reasons are `it has been still`, `a changing area has outgrown it`,
`the client cannot decode it` and `encoding failed`, with the error.
At debug, the display channel also logs each client report and each
bitrate change. The [metrics](#metrics) line counts streams.

**Patents.** The H.264 encoder is Cisco's openh264, whose code is
BSD-2-Clause. Andris builds it from source, as ryll does, and a build
from source carries none of Cisco's patent licence, which covers only
Cisco's own binary downloaded onto the user's machine. Whether
andris's packages ship the compiled encoder is undecided. This is a
statement of the position, not legal advice; if H.264 patents matter
to you, check them for your use.

### Not supported

- Clipboard images and the `PRIMARY` selection, and any resize the X
  server refuses.
- Video streams for clients without H.264, and more than one stream
  at a time.
- The desktop's volume and mute (see [Audio](#audio)), the client's
  volume, and a microphone.
- One client at a time.

### Known client gaps

Two quirks of ryll shape what andris sends:

- Ryll waits for a ping after the main channel's `INIT` before it asks
  for the channel list, so andris sends one straight after `INIT`
  ([ryll#502](https://github.com/shakenfist/ryll/issues/502)).
- Ryll stops reading the channel list at the inputs channel, so andris
  lists inputs last
  ([ryll#509](https://github.com/shakenfist/ryll/issues/509)); any
  channel listed after it would never be opened.

Ryll's protocol crate also does not yet name the inputs channel's
`KEY_SCANCODE` capability, so andris defines the constant itself
([ryll#503](https://github.com/shakenfist/ryll/issues/503)).

## Audio

Andris sends the desktop's sound to the client on the playback
channel. A client that links the channel hears whatever the desktop
plays.

**What is captured.** Andris records the monitor of the session's
default sink, which carries whatever that sink plays, over the
PulseAudio protocol. PipeWire's `pipewire-pulse` serves the same
protocol, so one code path covers PulseAudio and PipeWire, and an
xrdp sink module loaded into either (see
[xrdp.md](xrdp.md#sound)). `--audio-source NAME` names another
PulseAudio source instead, such as `xrdp-sink.monitor`; list them with
`pactl list short sources`. `--no-audio` captures nothing and leaves
the playback channel out of the channel list, and a client that asks
for it anyway is refused.

Andris resolves the default sink when the playback channel links. If
the default sink changes during a session, andris keeps the monitor
it opened until the client's playback channel reconnects.

Capture runs only while a client's playback channel is open. With
nobody listening, andris does not hold the sound server or keep a sink
awake.

**Codecs.** Sound is 48 kHz stereo in frames of 10 ms. A client that
advertises the Opus capability gets Opus at 96 kbit/s. Any other
client gets raw 16-bit PCM. Andris chooses once, when the channel
links.

**Silence.** A monitor delivers digital silence when nothing plays.
Andris sends `STOP` after one second in which every sample is within
8 of zero, and `START`, then that frame, as soon as sound returns. An
idle desktop costs the client nothing, and a client such as
spice-gtk gets a real stop. A paused or muted source is silence, so it
stops the stream too.

**Latency and sync.** Andris stamps audio and video with one clock,
the session's elapsed time. Video is stamped 80 ms ahead. spice-gtk
(`remote-viewer`) sets its clock from the audio it plays, and so
delays video to match it. Ryll plays audio and shows video as each
arrives, with no sync between them
([ryll#575](https://github.com/shakenfist/ryll/issues/575)), and does
not bound how far its playback ring can drift
([ryll#576](https://github.com/shakenfist/ryll/issues/576)). Andris
paces nothing itself: it sends each frame as the sound server delivers
it.

**A slow client loses sound rather than gaining latency.** Capture
hands frames to the channel through a queue of eight, 80 ms. When the
queue is full andris drops the oldest frame and counts it. The
playback socket also limits how much unsent data it holds, so a
stalled client does not make the kernel buffer seconds of sound that
would play late. The client's own receive buffer is out of andris's
reach.

**Muting the local speakers.** What it does to the remote stream
depends on the sink the monitor belongs to.

- A null sink, or any sink that applies volume in software: the
  monitor follows the sink's volume and mute. Muting it, or turning it
  to half, silences or halves the remote stream.
- The xrdp sink: the monitor ignores both. The remote stream plays at
  full volume however the sink is set.
- A real sound card under PipeWire: muting it silences the remote
  stream, as the null sink does. This was seen on an HDA card;
  whether its volume reaches the stream was not checked, and other
  cards may differ.

**Volume and mute are not forwarded.** Andris advertises the volume
capability but sends neither `VOLUME` nor `MUTE`, and does not apply
the client's volume to the desktop. The client plays at its own
volume.

**Without a sound server.** Andris needs `libpulse0` at run time. If
it cannot reach a sound server, or the source is gone, it logs one
warning each time a client links the channel and carries on: the playback channel stays
linked and silent, every other channel works, and capture is tried
again each time a client links the channel. Where PulseAudio is
installed and none is running, libpulse may start a daemon of its
own. Set `PULSE_SERVER` to name a server, or use `--no-audio`, if that
is not wanted.

**Feedback.** A SPICE client running on the desktop andris captures
plays the sound back into the sink andris records, which sends it to
the client again, and so on without end. Run the client elsewhere, or
give it no sound server of its own. `make e2e` points ryll at an
unreachable `PULSE_SERVER` for this reason.

The [metrics](#metrics) line counts audio frames, their bitrate, how
long Opus took to encode them, how many were dropped, and how many
times the stream started and stopped.

## Metrics

While a client is receiving frames, andris logs one line at info every
10 seconds, and one more when the session ends, for the whole session.
Nothing is logged for an interval in which no frame, refresh, stream
frame or audio frame was sent, so an idle desktop is silent. A line reads:

```
metrics: 12.4 fps (1 refreshes), draws lz4=118 bitmap=0 jpeg=6, display 842 kbit/s, capture 1.8 ms, encode 3.2 ms, ack lag 4.1 ms, ping 0.6 ms, scrub tiles 0, changed 31.5% of captured, streams created=0 destroyed=0, stream frames 0 at 0 kbit/s, stream encode - ms, server drops 0, stream target -, audio frames 0 at 0 kbit/s, opus encode - ms, audio drops 0, audio starts=0 stops=0
```

- `fps` is frames per second over the interval, and `refreshes` how
  many of them were lossless resends of JPEG areas.
- `draws` counts the rectangles sent, by encoding.
- `display` is the display channel's output.
- `capture` and `encode` are mean milliseconds per desktop capture and
  per comparing-and-encoding pass.
- `ack lag` is the mean time from sending the message that completes
  an acknowledgement window to the client's `ACK`; it rises when the
  client or the network falls behind.
- `ping` is the latest round trip to the client.
- `scrub tiles` is how many tiles the periodic re-read found changed
  that the X server's damage reports had not; a steadily nonzero
  figure means damage is being missed.
- `changed` is the share of captured pixels that had actually
  changed; a low figure means damage reports are coarser than the
  real changes.
- `streams created` and `destroyed` count [video streams](#video-streams)
  started and ended in the interval. `stream frames` is how many
  stream frames were sent, and `at` what `kbit/s`; they are not draws,
  nor counted in `fps`.
- `stream encode` is the mean milliseconds to capture and encode a
  stream frame.
- `server drops` is how many stream frames were skipped because the
  client had too much unacknowledged.
- `stream target` is the live stream's target bitrate, or `-` with
  none. Many creations and destructions in one interval mean streams
  are flapping.
- `audio frames` is how many 10 ms frames the playback channel sent,
  and `at` what `kbit/s`.
- `opus encode` is the mean milliseconds to encode a frame; it stays
  `-` for a client that gets raw PCM.
- `audio drops` is how many frames capture dropped because the client
  fell behind.
- `audio starts` and `stops` count the `START` and `STOP` messages
  sent; the silence rule makes them come in pairs.

A `-` means there was nothing to average in the interval.

### High CPU use

On a still desktop andris should use very little: no frames are
captured, and the only work is the scrub's. If it uses more, or a busy
desktop uses more than you expect, look at the [metrics](#metrics)
line and the startup log:

- The startup log says how the screen is captured. `GetImage` means
  every pixel crosses the X socket, which costs far more than MIT-SHM;
  the reason it gives (no MIT-SHM 1.2, a TCP display, a refused
  segment) is the thing to fix. A local display is the usual cure.
- `capture` is the time to read the pixels and `encode` the time to
  compare and compress them. A `capture` of many milliseconds with
  `GetImage` is the socket; with MIT-SHM it is the X server being
  slow, such as under software rendering. A large `encode` means a lot
  of pixels are changing, or a lot are being sent as JPEG.
- A high `fps` that sits at the cap (about 30 at the default) is a
  desktop that never stops changing, such as a video that is not
  streaming (see [Video streams](#video-streams)). Raising
  `--frame-interval-ms` lowers the frame rate, and so the CPU use, at
  the price of smoothness; `--jpeg-quality 0` removes JPEG's encode
  cost, and with it any streaming. A video that streams costs the H.264
  encode instead, shown as `stream encode`.
- A steadily nonzero `scrub tiles` means the X server is missing
  damage, so the scrub is doing real work; a `changed` figure that is
  low means damage reports are coarse and most of each capture is
  unchanged.
- No metrics line at all means no frame or refresh was sent in the interval, so
  any CPU use is the scrub's.

## Connecting with ryll

Use a ryll built from its `develop` branch, at or after the revision
andris's `Cargo.lock` pins, until a ryll release after 0.1.7 carries
the same changes. Ryll 0.1.7 and earlier decode LZ4 images differently
from how spice-server and andris send them (shakenfist/ryll#475): the
pointer moves, but the display never paints, and ryll logs
`LZ4 unsupported spice format` for every draw.

Ryll's `--direct` option has no password, so a ticket needs a
virt-viewer `.vv` file. Write one:

```ini
[virt-viewer]
type=spice
host=127.0.0.1
port=5900
password=a-secret
```

and open it:

```bash
ryll --file andris.vv
```

`password` is the contents of the ticket file. The file holds the
ticket, so keep it private (`chmod 600`). Ryll's documentation covers
the `.vv` keys it understands for TLS. If the machine running
andris is remote, forward its port instead of opening the listener:
`ssh -L 5900:127.0.0.1:5900 host`.

## Fronting andris with kerbside

Kerbside can front andris with its static console source, which maps
a console entry onto a SPICE server it does not manage. The fields
map to andris's options like this:

| Kerbside field | Andris |
|----------------|--------|
| `hypervisor_ip` | The address andris is reachable at; `127.0.0.1` if kerbside runs on the same machine |
| `insecure_port` | The port of `--listen`, such as `5900` |
| `secure_port` | The port of `--tls-listen`, optional; kerbside also needs the source's `ca_cert` to verify the certificate |
| `ticket` | The contents of the ticket file |

```yaml
- source: andris
  type: static
  consoles:
    - uuid: "6f4e2c1a-0000-0000-0000-0000000000a1"
      name: "xrdp-desktop"
      hypervisor: "localhost"
      hypervisor_ip: "127.0.0.1"
      insecure_port: 5900
      ticket: "a-secret"
```

Kerbside's own documentation covers the source and its
other fields, and warns that the static source is not intended for
production use:
[console-sources.md](https://github.com/shakenfist/kerbside/blob/develop/docs/console-sources.md).
Andris sends only message types that kerbside's allowlist accepts,
so a session through kerbside needs no special configuration.
Every connection through kerbside comes from kerbside's address, so
the limit of 4 handshakes in progress per address applies to all of
kerbside's clients together; that is enough for one client linking
its channels in parallel.
