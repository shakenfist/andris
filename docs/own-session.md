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
`-noreset` keeps the server up when its last client leaves, so
restarting the desktop does not drop the SPICE clients with it. MIT-SHM
works on Xvfb, so andris captures through a shared memfd as it would on
any local display.

Then a desktop, with `DISPLAY` naming the display. Running it the way a
display manager would gets the session the same setup a console login
has:

```bash
DISPLAY=:20 /etc/X11/Xsession xfce4-session
```

An Xorg server with the `dummy` driver works too, and gives a real Xorg
with a configurable virtual size, at the price of a configuration file,
a virtual console and the setuid wrapper. Xvfb is the simpler of the
two, and the desktop is drawn in software either way.

## As systemd user units

The shape which matches [`andris@.service`](xrdp.md#installing) is two
more template units keyed by the display number, so that the display,
the desktop and andris are one chain:

- an X server unit, `Restart=always`, running the `Xvfb` line above;
- a session unit which `BindsTo=` and `After=` it, running the
  `Xsession` line, so that starting the session starts the display, and
  a display which dies takes its session with it;
- `andris@20.service`, as [xrdp.md](xrdp.md) installs it, unchanged.

Andris needs no ordering of its own: it exits 1 when it cannot reach the
display, and its unit restarts it with a backoff until the display
answers.

The session unit is worth restarting even on a clean exit. Logging out
of a desktop which has no monitor otherwise leaves an X server nothing
draws on, and no way back in but a shell.

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
  and treat it as a ceiling rather than a size. An idle desktop at that
  ceiling costs nothing, because andris captures only what the desktop
  damages.
- **The desktop does not come and go.** There is no session to log out
  of from a client, nothing orphans a session, and the display number
  never changes under the unit, so the instance name is right for good
  (compare [xrdp.md](xrdp.md#when-the-session-ends)).
- **No RDP client is needed to create it.** The desktop is the user
  manager's, so with lingering enabled it exists from boot.
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
its own X server, and cannot attach to one it did not start, so RDP
reaches a second desktop for the same user. The two desktops then share
one sound server and one keyring, which is fine as a way back in to a
machine, and not a way to work in both at once.

**One instance at a time, unless each gets its own port.** The unit
reads `ANDRIS_ARGS` from `%E/andris/andris.env`, which every instance
shares, so `andris@10` and `andris@20` would both try to listen on the
same address and only the first would start. Give each instance a
drop-in (`systemctl --user edit andris@20`) with its own `--listen`, or
run one at a time.
