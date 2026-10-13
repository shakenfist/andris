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
to the machine whose desktop you want to export.

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

A connection opens four channels, and ryll opens all four:

| Channel | What andris sends or does |
|---------|---------------------------|
| main | Opens the session, lists the other channels, applies the client's mouse mode request, pings every 30 seconds, and plays the guest agent (below), which carries the clipboard and desktop resizing |
| display | The desktop as images (below) |
| inputs | Applies the client's keys and pointer events to the desktop, and reports the desktop's lock-key state |
| cursor | The pointer's shape as it changes, as an alpha cursor |

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
such as a video, stays JPEG for as long as it changes.

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

### Not supported

- Clipboard images and the `PRIMARY` selection, and any resize the X
  server refuses.
- No video streams: the desktop is always sent as images.
- No audio.
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

## Metrics

While a client is receiving frames, andris logs one line at info every
10 seconds, and one more when the session ends, for the whole session.
Nothing is logged for an interval in which no frame or refresh was sent, so an
idle desktop is silent. A line reads:

```
metrics: 12.4 fps (1 refreshes), draws lz4=118 bitmap=0 jpeg=6, display 842 kbit/s, capture 1.8 ms, encode 3.2 ms, ack lag 4.1 ms, ping 0.6 ms, scrub tiles 0, changed 31.5% of captured
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
  desktop that never stops changing, such as a video. Raising
  `--frame-interval-ms` lowers the frame rate, and so the CPU use, at
  the price of smoothness; `--jpeg-quality 0` removes JPEG's encode
  cost.
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
