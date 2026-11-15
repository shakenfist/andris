# Running andris on a desktop of its own

Andris never starts a desktop: it attaches to an X display which is
already there (see [running.md](running.md)). Usually that display
belongs to something else -- the session behind xrdp, which
[xrdp.md](xrdp.md) covers. It does not have to. A machine with no
monitor, or one reached only over SPICE, can run an X server and a
desktop session of its own, for andris to export, under the same user
manager that runs andris. The desktop then exists whether or not any
client is connected, and comes up at boot.

This page is a recipe, not a feature: nothing here is part of andris,
and andris cannot tell this display from any other.

## What it takes

An X server which meets the checks in
[running.md](running.md#exporting-an-x-display), and a desktop session
on it. Xvfb meets them at 24 bits, needs no privileges, no virtual
console and no configuration file, and is the server andris's
end-to-end test runs against, so it is the one to reach for:

```bash
xauth add :20 . "$(mcookie)"
Xvfb :20 -screen 0 3840x2160x24 -auth "$HOME/.Xauthority" -nolisten tcp -noreset -dpi 96
```

The cookie goes in the default authority file, which is where andris
looks when `XAUTHORITY` is unset, so nothing else needs configuring.
`xauth add` replaces any entry the display already has, so writing a
fresh cookie before every start of the server is harmless. Without
`-noreset`, an X server whose last client disconnects resets: it stays
running, but reinitialises, losing among other things the screen size a
client resized it to. That happens if the desktop and andris both go at
once; `-noreset` keeps the server's state across it. MIT-SHM works on
Xvfb, so andris captures through a shared memfd as it would on any
local display.

Then a desktop, with `DISPLAY` naming the display. Running it the way a
display manager would gets the session the same setup a console login
has:

```bash
DISPLAY=:20 /etc/X11/Xsession xfce4-session
```

`/etc/X11/Xsession` is Debian and Ubuntu's wrapper; Fedora and RHEL
have it at `/etc/X11/xinit/Xsession`.

An Xorg server with the `dummy` driver works too, and gives a real Xorg
with a configurable virtual size, at the price of a configuration file,
a virtual console and the setuid wrapper. Xvfb is the simpler of the
two, and the desktop is drawn in software either way.

## As systemd user units

Two more template units keyed by the display number, the shape of
[`andris@.service`](xrdp.md#installing), make the display, the desktop
and andris one chain. The X server, in
`~/.config/systemd/user/xvfb@.service`:

```ini
[Unit]
Description=Xvfb on display :%i
Upholds=xsession@%i.service

[Service]
ExecStartPre=/bin/sh -c 'xauth -f %h/.Xauthority add :%i . "$$(mcookie)"'
ExecStart=Xvfb :%i -screen 0 3840x2160x24 -auth %h/.Xauthority -nolisten tcp -noreset -dpi 96
Restart=always
RestartSec=2s

[Install]
WantedBy=default.target
```

The desktop, in `~/.config/systemd/user/xsession@.service`:

```ini
[Unit]
Description=Desktop session on display :%i
BindsTo=xvfb@%i.service
After=xvfb@%i.service
StartLimitIntervalSec=0

[Service]
Environment=DISPLAY=:%i
ExecStart=/etc/X11/Xsession xfce4-session
Restart=always
RestartSec=2s
```

Then, with `andris@.service` installed as [xrdp.md](xrdp.md#installing)
describes, and lingering enabled (`loginctl enable-linger`) so that the
user manager starts at boot rather than at the first login:

```bash
systemctl --user daemon-reload
systemctl --user enable --now xvfb@20 andris@20
```

How the pieces fit:

- **The cookie is the X server's to write.** `ExecStartPre=` runs the
  `xauth add` above before each start, so a restarted Xvfb gets a fresh
  cookie and the desktop and andris read it when they reconnect. `$$` is
  systemd's escape for a literal `$`.
- **The display brings the desktop with it.** The session `BindsTo=`
  and is ordered `After=` the display, so starting the session starts
  the display, and a display which stops takes the session with it.
  `BindsTo=` only ever stops the session, though: when the display
  restarts, the session it stopped stays stopped. `Upholds=` in the X
  server unit is what brings it back. It starts the session whenever
  the display is up without one: after the display restarts, and after
  logging out of the desktop, which would otherwise leave an X server
  nothing draws on and no way back in but a shell. The cost is that
  stopping the session by hand does not stick while the display runs;
  stop `xvfb@20`, which stops both, instead. `Upholds=` needs systemd
  249, older than the 254 which `andris@.service` needs.
- **The desktop retries until the display answers.** Xvfb tells systemd
  nothing about when it is ready, so the first start of the session can
  find no display there yet. The session restarts on any exit, and
  `StartLimitIntervalSec=0` stops systemd giving up on it while it does.
- **Andris needs no ordering of its own.** It exits 1 when it cannot
  reach the display, and its unit restarts it with a backoff until the
  display answers.

Enable the units for whichever display number is free. `:0` and `:1`
usually belong to the console's display manager, and xrdp hands out
`:10` upwards, so a number well clear of both, such as `:20`, keeps all
three able to coexist.

## What is different from the xrdp session

- **Resizing works.** xorgxrdp refuses every screen-size change from an
  X client (see [xrdp.md](xrdp.md#resizing)), so on xrdp a SPICE
  client's "resize guest to window" is always refused. Xvfb allows it,
  below the size the server started with, so andris applies it: pick an
  `-screen` size as large as the biggest client window you will give it,
  and treat it as a ceiling rather than a size. Frames cost only what
  the desktop damages, but the ceiling is not free: Xvfb's framebuffer
  is sized by it (about 32 MiB at 3840 by 2160), and a client which
  does not resize the desktop gets all of it, which andris re-reads
  every four seconds even when nothing moves (see
  [running.md](running.md#high-cpu-use)). So
  pick the smallest ceiling that covers your largest client window. A
  ceiling past 3840 by 2160 also costs full-screen video its stream,
  since that is the most the encoder takes, and a larger video goes as
  JPEG (see [running.md](running.md#video-streams)).
- **The desktop does not come and go.** There is no session to log out
  of from a client, nothing orphans a session, and the display number
  never changes under the unit, so the instance name is right for good
  (compare [xrdp.md](xrdp.md#when-the-session-ends)).
- **No RDP client is needed to create it.** The desktop is the user
  manager's, so with lingering enabled it exists from boot.
- **Nothing asks for a password in front of it.** On xrdp, an RDP login
  stands between a client and the session. Here the desktop is logged in
  from boot, and a SPICE client with the ticket is at it, keyring and
  all. Treat the ticket as the password to the desktop: keep it private,
  rotate it if it may have leaked (see
  [running.md](running.md#the-ticket-file)), and beyond a single-user
  machine's loopback, use TLS or kerbside, as
  [xrdp.md](xrdp.md#on-a-shared-host) describes.
- **Sound needs nothing special.** Where an xrdp session has no sound
  card at all until something gives it one
  ([xrdp.md](xrdp.md#sound)), a desktop of its own is an ordinary user
  session on the machine: if PipeWire or PulseAudio is running with a
  sink, andris records that sink's monitor as it does anywhere else
  (see [running.md](running.md#audio)). A machine with no sound card at
  all needs a null sink to have anything to capture.
- **A systemd user unit is not a login.** The desktop gets no logind
  session and no seat, so polkit actions which require an active
  session, such as mounting removable media, may refuse or ask for a
  password. Where that matters, the session belongs in a *system* unit
  with `User=` and `PAMName=login`, which does create a logind session,
  rather than in a user unit.

## Alongside xrdp

The two are not exclusive: andris exports whichever display its instance
names, so a machine can keep its xrdp setup and run a desktop of its own
on another display at the same time. Two things to keep in mind.

**An RDP client gets the xrdp session, not this desktop.** xrdp starts
its own X server, and cannot attach to one it did not start, so an RDP
client gets xrdp's own session: a second, separate desktop for the same
user. The two desktops then share one sound server and one keyring,
which is fine as a way back in to a machine, and not a way to work in
both at once.

**One instance at a time, unless each gets its own port.** The unit
reads `ANDRIS_ARGS` from `%E/andris/andris.env`, which every instance
shares, so `andris@10` and `andris@20` would both try to listen on the
same address and only the first would start. Setting `ANDRIS_ARGS` with
`Environment=` in a drop-in does not help: systemd applies an
`EnvironmentFile=` over `Environment=`, so the shared file wins. A
second environment file does, since a later file wins over an earlier
one. Add one per instance to every instance with a drop-in for the
template, `systemctl --user edit andris@`:

```ini
[Service]
EnvironmentFile=-%E/andris/andris-%i.env
```

and give an instance its own address in its file,
`~/.config/andris/andris-20.env`:

```bash
ANDRIS_ARGS=--listen 127.0.0.1:5920
```

That replaces the shared `ANDRIS_ARGS` rather than adding to it, so
repeat any other options it carries. Or run one instance at a time.
