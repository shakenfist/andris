# Plans index

This page summarises every planning document in chronological order. Master
plans decompose work into numbered phases, each with its own detailed plan
file.

## Master plans

| Date | Plan | Intent | Status | Phases |
|------|------|--------|--------|--------|
| 2026-10-08 | [X11 desktop over SPICE](PLAN-x11-desktop.md) | A Rust SPICE server that exports an existing X11 display (sharing an existing xrdp session), reached from ryll through kerbside, to replace RDP for dogfooding | Not started | 0. Join the consistency audit, 1. Build and CI scaffold, 2. Server-role wire types (ryll), 3. Image encoders (ryll), 4. Server skeleton, 5. Damage and flow control, 6. Sharing the xrdp session, 7. Agent: resize and clipboard, 8. Video streams, 9. Audio, 10. Packaging and dogfood soak, 11. Push audit (phase plans pending) |
