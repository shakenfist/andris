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
| 2026-10-08 | [X11 desktop over SPICE](PLAN-x11-desktop.md) | A Rust SPICE server that exports an existing X11 display (sharing an existing xrdp session), reached from ryll through kerbside, to replace RDP for dogfooding | In progress | [0. Join the consistency audit](PLAN-x11-desktop-phase-00-audit.md), [1. Build and CI scaffold](PLAN-x11-desktop-phase-01-scaffold.md), [2. Server-role wire types (ryll)](PLAN-x11-desktop-phase-02-wire-types.md), [3. Image encoders (ryll)](PLAN-x11-desktop-phase-03-encoders.md), 4. Server skeleton, 5. Damage and flow control, 6. Sharing the xrdp session, 7. Agent: resize and clipboard, 8. Video streams, 9. Audio, 10. Packaging and dogfood soak, 11. Push audit (phase plans pending) |
