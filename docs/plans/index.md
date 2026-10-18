# Plans index

This page summarises every planning document in chronological order. Master
plans decompose work into numbered phases, each with its own detailed plan
file.

New plans should follow the structure in `PLAN-TEMPLATE.md` at the repo
root. For pre-push audits of our own work, including the push-audit phase
that ends every master plan, see `PUSH-AUDIT.md`.

The `Status` column below holds exactly one term from the vocabulary in
`PLAN-TEMPLATE.md`: `Proposed`, `Not started`, `In progress`, `Blocked`,
`Complete`, `Abandoned` or `Superseded`. Detail about a plan's state
belongs in the plan itself rather than in this table.

## Master plans

| Date | Plan | Intent | Status | Phases |
|------|------|--------|--------|--------|
| 2026-10-08 | [X11 desktop over SPICE](PLAN-x11-desktop.md) | A Rust SPICE server that exports an existing X11 display (sharing an existing xrdp session), reached from ryll through kerbside, to replace RDP for dogfooding | In progress | [0. Join the consistency audit](PLAN-x11-desktop-phase-00-audit.md), [1. Build and CI scaffold](PLAN-x11-desktop-phase-01-scaffold.md), [2. Server-role wire types (ryll)](PLAN-x11-desktop-phase-02-wire-types.md), [3. Image encoders (ryll)](PLAN-x11-desktop-phase-03-encoders.md), [4. Server skeleton](PLAN-x11-desktop-phase-04-server-skeleton.md), [5. Damage and flow control](PLAN-x11-desktop-phase-05-damage.md), [6. Sharing the xrdp session](PLAN-x11-desktop-phase-06-sharing.md), [7. Agent: resize and clipboard](PLAN-x11-desktop-phase-07-agent.md) (a ryll pull request for headless clipboard and resize, then the agent in andris: text on CLIPBOARD both ways, and RandR resize where the X server allows it, which xorgxrdp does not), [8. Video streams](PLAN-x11-desktop-phase-08-video.md) (a ryll pull request moving its H.264 encoder into the compression crate and advertising H.264 while preferring MJPEG, then photographic areas that keep changing served as one openh264 stream per channel, with report-driven rate control and a lossless repair when the stream ends), [9. Audio](PLAN-x11-desktop-phase-09-audio.md) (a ryll pull request adding playback wire types and a `playback` section to its control socket's `stats`, then the desktop's default sink monitor captured over the PulseAudio protocol, which PipeWire also serves, and sent as 10 ms Opus frames that start and stop with the sound; the desktop's volume and mute are not yet forwarded), 10. Packaging and dogfood soak, 11. Human review, 12. Push audit (phase plans pending) |
