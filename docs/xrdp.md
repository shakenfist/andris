# Running andris alongside xrdp

How to run andris as a service next to xrdp, so that the desktop an
RDP client sees can also be reached over SPICE, from
[ryll](https://github.com/shakenfist/ryll) directly or through
kerbside. The options, the ticket file and TLS are described in
[running.md](running.md); this page is about the service, and about
what happens when an RDP client and a SPICE client use the same
desktop.

Andris does not replace xrdp or take over its session. xrdp starts an
Xorg server for the user (xorgxrdp), and the RDP client draws on it.
Andris connects to that same X server as an ordinary client, so both
protocols show the one desktop. Either can come and go without the
other noticing.

## Installing

Install andris from PyPI, as the user whose desktop it will export:

```bash
pipx install andris        # or: pip install --user andris
andris install-unit
```

The wheel carries the compiled binary, with the H.264 encoder built
in, for x86_64 and aarch64 Linux with glibc 2.28 or newer. It needs
nothing else to run, though sound needs libpulse (see
[Sound](#sound)). `andris install-unit` then sets up the service. It
writes the unit to `$XDG_CONFIG_HOME/systemd/user/andris@.service`
(`~/.config/systemd/user/` when that is unset), naming the absolute
path of the binary that ran it, and makes `~/.config/andris/`, with
mode 0700, if it is missing. If `~/.config/andris/ticket` does not
exist, it writes a random ticket there, readable only by the user. It
never runs systemctl: it prints the commands to run next, which
[Starting it](#starting-it) explains.

The unit is a systemd *user* unit, so it runs as the session's user,
which is the user the X server accepts. It needs systemd 254 or newer,
for its `RestartSteps=` and `RestartMaxDelaySec=` settings; Debian 13
has 257. It is a template: the part after the `@` in the instance name
is the display number it exports. Its source is
[`andris@.service`](https://github.com/shakenfist/andris/blob/develop/packaging/systemd/andris@.service).

`install-unit` reports whether it wrote, replaced or left the unit
unchanged, and always overwrites it, so run it again whenever the
binary moves, after reinstalling andris with a different pipx or pip,
say. Do not edit the installed unit: the next run replaces it. Put
local changes in a drop-in, with `systemctl --user edit andris@`.

A running andris keeps running the binary it started from, so after an
upgrade restart each instance to run the new one:

```bash
pipx upgrade andris
systemctl --user restart andris@10
```

Systemd cannot run a binary whose path holds a quote, a backslash or a
control character, so `install-unit` refuses one; install andris
elsewhere if yours does. It also refuses to run as root, sudo
included: run it as the user whose desktop andris will export.

It reports an `andris/` directory that is open to other users, and a
ticket andris would refuse (see
[the ticket file](running.md#the-ticket-file)), rather than changing
them.

### Building from source

To run a build of your own instead, build the release binary (see
[running.md](running.md#installing-and-building)) and run `install-unit` with it:

```bash
make release
target/release/andris install-unit
```

The unit then runs that binary, wherever the checkout is. The binary
is dynamically linked, so build it for, or on, a machine like the one
that will run it.

## Configuring

The unit reads two things from `~/.config/andris/`, which it finds
through systemd's `%E`, the user manager's `$XDG_CONFIG_HOME`.

The ticket file, `ticket`, holds the password SPICE clients must
present. `andris install-unit` made one if there was none. It must be
private to the user. To replace it, for instance after it may have
leaked, write a new one as you would for a manual run:

```bash
(umask 077; head -c 24 /dev/urandom | base64 > ~/.config/andris/ticket)
```

`andris.env` is optional. It is an environment file, and the unit
reads two variables from it. It must not hold secrets: the ticket lives
only in the ticket file, and there is no option to give it any other
way.

```bash
# ~/.config/andris/andris.env
ANDRIS_ARGS=--listen 127.0.0.1:5900
XAUTHORITY=/home/alice/.some-other-cookie
```

`ANDRIS_ARGS` is appended to andris's command line, so it is where
options such as `--listen`, `--tls-listen`, `--tls-cert` and
`--tls-key` go. It is split on whitespace, so a path containing a
space cannot be given there. `XAUTHORITY` is needed only if the X
server's cookie is not in `~/.Xauthority`. xrdp's Xorg uses
`~/.Xauthority`, which is also where andris looks when `XAUTHORITY` is
unset, so a stock xrdp setup needs no `XAUTHORITY` at all. Leave the
file out altogether if you need neither.

Make any TLS certificate and key before enabling the instance. Andris
reads them, and the ticket, at startup, and a missing or unreadable
one is a refused configuration: the instance stops and stays stopped
(see the exit statuses below) until you fix it and start it again with
`systemctl --user start andris@10`.

## Starting it

First find the display number of the xrdp session. The Xorg process
for it has the display as an argument, and its socket is in
`/tmp/.X11-unix`:

```console
$ pgrep -u "$USER" -a Xorg
2417 Xorg :10 -auth .Xauthority -config xrdp/xorg.conf -noreset -nolisten tcp
$ ls /tmp/.X11-unix
X10
```

`X10` is display `:10`. xrdp gives a user's session the same number
each time while the session lives, so you name it once. `andris
install-unit` printed these two commands, with the number of the
display `DISPLAY` named if it named a local one; use the number you
found:

```bash
systemctl --user daemon-reload
systemctl --user enable andris@10
systemctl --user restart andris@10
```

`enable` makes the instance start whenever the user's systemd manager
does, and `restart` starts it straight away. `install-unit` prints
`restart` rather than `enable --now` because `--now` does nothing to an
instance that is already running: after a reinstall, the old binary
would carry on serving, from its deleted file if the reinstall removed
it, under a unit that names the new one. `restart` starts a stopped
instance, and replaces a running one, dropping any client connected to
it. Check it, and read its log, with:

```bash
systemctl --user status andris@10
journalctl --user -u andris@10
journalctl --user -u andris@10 -f     # follow it
```

Andris logs to stderr, which the journal captures. Set `RUST_LOG` in
`andris.env` to change the log level. At `debug` and `trace` andris
may log details of the client's input, such as the keys it sends, and
an eight-digit hash of each clipboard text, from which a short text
such as a password can be found by guessing, to the journal, which
members of the `adm` and `systemd-journal` groups can read; use those
levels only while troubleshooting.

Andris's exit status says why it stopped:

| Status | Meaning | What systemd does |
|--------|---------|-------------------|
| 0 | Stopped by SIGINT or SIGTERM | Nothing; the stop was asked for |
| 1 | The desktop went away, or another failure while running | Restarts it, as described below |
| 2 | The configuration was refused | Does not restart it; read the journal for the reason, fix it, and start the instance again |

Then connect ryll to the address andris listens on, with the ticket
from the ticket file, as described in [running.md](running.md). To put
kerbside in front, see
[Fronting andris with kerbside](running.md#fronting-andris-with-kerbside).

## On a shared host

Loopback is not private to one user: every account on the host can
reach `127.0.0.1`. Bear that in mind on a host with more than one
user.

- **Each user needs their own port.** Every instance listens on
  `127.0.0.1:5900` unless told otherwise, so only one could start.
  Give each user a distinct `--listen` port in their `andris.env`.
- **A plain SPICE client cannot tell andris from an impostor.** If
  another local user binds the port first, while andris is not running
  (between sessions, or while systemd waits to restart it), a client
  connecting there talks to their program, and hands it the ticket. On
  a shared host, use TLS (`--tls-listen`, `--tls-cert` and
  `--tls-key`, described in [running.md](running.md)) even on
  loopback, so the client can check it is talking to andris, or reach
  andris only through kerbside.
- **Rotate the ticket if it may have leaked.** Write a new ticket file
  as in [Configuring](#configuring), and restart the instance with
  `systemctl --user restart andris@10`.

## When the session ends

**Logging out.** When the xrdp session ends, its X server goes away.
Andris notices, closes the SPICE client's connection, logs
`the desktop has gone, so andris is stopping`, and exits with status 1
and an error naming the reason. What happens next depends on whether the user's
systemd manager outlives the session:

- **Usually it does not.** Unless lingering is enabled for the user
  (`loginctl show-user "$USER" -p Linger`), the user's systemd manager
  stops when their last session ends, and stops andris with it. At the
  next login, xrdp starts a new session and the manager starts andris
  again, because the instance is enabled.
- **If it does,** because of lingering or another login, systemd treats
  the exit as a failure and restarts andris after 2 seconds. While
  there is no X server, each restart fails to connect and exits again,
  and systemd waits a little longer each time, up to 60 seconds. Once
  xrdp starts a new session on the same display, the next restart
  connects.

Either way, andris may start before the new session's X server is
ready; it then fails to connect, and systemd retries until it can.
Nothing needs to be done by hand. A SPICE client has to reconnect,
since its connection was closed, and it sees the new session's
desktop. If the new session lands on a different display number, the
instance name no longer matches: enable the instance for the new
number, and disable the old one with
`systemctl --user disable --now andris@N`, where `N` is the old number.
An instance left pointing at the old number is kept off another user's
session there only by X access control, since that session's cookie is
not the one andris holds. For the same reason, never pair andris with
`xhost +`, which turns that access control off.

**Disconnecting RDP.** Closing the RDP client, or losing its
connection, does not end the session: xrdp keeps the session and its
X server running. Andris is unaffected and a SPICE client stays
connected and sees the desktop as the session left it. Reconnecting
the RDP client resumes the same session.

**Stopping the service.** `systemctl --user stop andris@10` sends
SIGTERM. Andris closes the client's connections, releases any key or
button the client still held, and exits with status 0, which systemd
does not restart.

## RDP and SPICE together

Both protocols type into the same X server, so what each does is
visible to the other. This is what to expect.

- **Keys arrive in order.** Keys typed over RDP and over SPICE, even
  alternating, each reach the desktop once and in the order they were
  sent. Applications that watch for keyboard mapping changes see a
  notification each time the typing switches from one to the other.
  That is how the X server treats two input devices, and is harmless
  to a program that refreshes its keymap when told.
- **Held modifiers combine.** A Shift held over RDP applies to a key
  typed over SPICE, and the other way round. Once each side releases
  its Shift, nothing is left stuck.
- **One keyboard layout, for both.** SPICE clients send scancodes, and
  the desktop's layout decides which characters they make, as it does
  for RDP. So the two always agree. An RDP client that reconnects
  asking for another layout makes xrdp load that layout into the
  running session, and from then on SPICE keys type through the new
  layout too: the same key that typed `y` types `z`. Andris needs no
  restart for this.
- **Lock keys follow whoever synchronised last.** Each client sets the
  desktop's Caps Lock, Num Lock and Scroll Lock to match its own when
  it synchronises. FreeRDP does so whenever its window takes the
  focus. Ryll does so when it connects, and sends Num Lock on and Caps
  Lock off. A lock set from one client therefore holds until the other
  synchronises. Currently ryll also repeats that state when its inputs
  channel has been idle for 10 seconds, so while a ryll client is
  connected an idle desktop is put back to Num Lock on and Caps Lock
  off after about 10 seconds, undoing a lock key pressed over RDP.
  This is a known ryll gap,
  [shakenfist/ryll#517](https://github.com/shakenfist/ryll/issues/517).
- **A resize reaches SPICE as a new surface.** When the RDP client
  resizes the session, or reconnects with another size, the X server
  changes size and andris tells the SPICE client about a new surface of
  that size. A client that handles a resize, as ryll does, follows it.

## Resizing

On xrdp the desktop follows the RDP client's size, and a resize asked
for over SPICE is refused. xorgxrdp does not let an X client change the
screen size: its RandR code accepts a size change only when it comes
from xrdp, as the RDP client's own, and answers anything else with
`BadMatch`, even when no RDP client is connected. Andris reads the size
back after asking, finds that nothing changed, and tells the SPICE
client so with an error reply; the desktop keeps the size it has, and
the log gets one warning per connection (see
[running.md](running.md#what-a-client-gets)). To change the desktop's
size, resize the RDP client's window or reconnect it with another size,
and the SPICE client follows as described above.

## Clipboard with RDP and SPICE together

The clipboard works with an RDP client connected. For RDP, xrdp's
`xrdp-chansrv` bridges the session's X clipboard to the client; for
SPICE, andris does. Both are ordinary X clipboard clients of the same
server, so text copied on either side reaches the desktop, and the
other bridge then offers it to its own client. Text only: UTF-8 on
`CLIPBOARD`, as in [running.md](running.md#what-a-client-gets).

- **Each copy crosses once.** With an RDP client and a SPICE client on
  one machine sharing one clipboard, as when both run on a Mac, a copy
  made in one reaches the other once rather than bouncing between the
  bridges. Andris drops an echo of the text that last crossed, in either
  direction, so copying the same text again sends nothing: the other
  side already has it.
- **The SPICE client gets every copy.** chansrv sends text to the RDP
  client only when it pastes; andris offers the SPICE client each copy
  as it is made, and ryll fetches it at once, so text copied in the
  session, passwords included, reaches the SPICE client unpasted (see
  [running.md](running.md#what-a-client-gets)).
- **After the RDP client disconnects**, chansrv may keep owning the
  clipboard and not answer requests for its text until the RDP client
  reconnects. Andris gives up on a fetch after 10 seconds, so the SPICE
  client's clipboard is never stuck, but text copied just before the
  disconnect may not reach it.

## Sound

Andris captures the monitor of the session's default sink and sends it
to SPICE clients (see [running.md](running.md#audio)). An xrdp session
has no sound card, so unless something gives it a sink, there is
nothing worth capturing. The same sink is what gives RDP clients
sound, so one install serves both.

Andris loads libpulse when it first needs it. Without it, andris logs
`cannot load libpulse, so andris serves without sound`, naming the
package to install (`libpulse0` on Debian and Ubuntu, `pulseaudio-libs`
on Fedora and RHEL), and offers clients no playback channel, as with
`--no-audio`. The rest of the desktop works.

Install the xrdp sink module for the session's sound server. On Debian
13 with PipeWire that is `pipewire-module-xrdp`:

```bash
sudo apt install pipewire-module-xrdp
```

Its loader runs from an XDG autostart entry
(`/etc/xdg/autostart/pipewire-xrdp.desktop`) when an xrdp session
starts. It acts only inside a session xrdp's sesman started. It loads
the module, then makes `xrdp-sink` and `xrdp-source` the session's
defaults. The session must run a desktop that processes autostart
entries, and the sound server must be up first. Debian 13 does not
package `pulseaudio-module-xrdp`, the module for a PulseAudio
session.

With `xrdp-sink` as the default, andris records `xrdp-sink.monitor`,
and sound played in the session reaches both clients. Check with
`pactl get-default-sink` as the session's user. Andris looks the
default up when a SPICE client's playback channel links, so a client
that connected before the sink became the default hears nothing until
it reconnects. To pin the source whatever the default is, start andris
with `--audio-source xrdp-sink.monitor`.

The xrdp sink's monitor ignores the sink's volume and mute, so the
SPICE client hears the sound at full volume whatever the RDP client or
the session sets. [Audio](running.md#audio) says what other sinks do.

## Trying it without a live session

Everything on this page is exercised by `make e2e-xrdp`, which runs a
real xrdp session in a throwaway container and never touches a real
one. See [development.md](development.md#the-xrdp-test-bed).
