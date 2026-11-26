# Serve one desktop to several clients at once

## Prompt

Before responding to questions or discussion points in this
document, explore the andris repository and the ryll crates it
builds on. Read the relevant source and ground your answers in
what the code actually does today. Do not speculate about either
codebase when you could read it instead. Flag any uncertainty
explicitly rather than guessing.

Andris is a SPICE *server*. Ryll (`shakenfist/ryll`) is the SPICE
client whose shared crates andris depends on, and kerbside
(`shakenfist/kerbside`) is the SPICE proxy that sits between them.
Key references:

- `andris/src/server/session.rs`: the session registry this plan
  changes, and the farewell that `42ef302` added.
- `ryll/src/app/reconnect.rs`: the client half of that farewell.
  `ModalVariant::ServerEnded` is what stops ryll reconnecting, and
  `ReconnectState::server_ended` is what keeps it stopped.
- `kerbside/rust/kerbside-proxy/src/caps.rs` and `allowlist.rs`:
  the per-channel message-type allowlist every message andris adds
  must already satisfy.
- `/srv/src-reference/spice/spice/` (spice-server, LGPL): a
  reference for *behaviour only*, and the source of the current
  one-client-at-a-time commitment. No code is translated from it.
- `/srv/src-reference/spice/spice-protocol/`: canonical wire
  definitions.

`ARCHITECTURE.md` carries the shape of the repository, `AGENTS.md`
the conventions, and `docs/plans/PLAN-x11-desktop.md` the design
commitments this plan deliberately reverses one of. Andris is
built with `make`, which runs cargo inside a Docker devcontainer;
cargo is never run on the host.

## Situation

Andris serves one client at a time. A main-channel link that
authenticates begins a session and replaces the one before it:
`RegistryState` holds `current: Option<Arc<Session>>`, and
`SessionRegistry::begin` swaps it (`andris/src/server/session.rs`).
A secondary channel must name the current session or be refused
with `BadConnectionId` (`server/link.rs:267`). That was the right
call for phase 4 of the X11 desktop plan, and it is written down
as a commitment in seven places:

| Where | What it says |
|-------|--------------|
| `docs/plans/PLAN-x11-desktop.md:122` | "**One client at a time.** A new connection replaces the old one, as spice-server does." |
| `docs/plans/PLAN-x11-desktop.md:125` | "Not covered: Wayland, multiple simultaneous viewers, …" |
| `ARCHITECTURE.md` | Design commitments, and the session-registry row of the component table |
| `README.md:34` | "It exports an X display to one client at a time" |
| `AGENTS.md:12` | "…to one client at a time" |
| `docs/index.md:13` | "channels to one client at a time" |
| `docs/running.md:364` | "Andris serves one client at a time. A client that authenticates replaces the one before it." |
| `docs/reading-guide.md:82` | "…serves one client at a time" |

### What went wrong in practice

The operator reaches one desktop from two machines: ryll on a
laptop, and a session through kasm. Dogfooding on 2026-11-17
produced this, from `andris@20`'s journal (local time; the service
was then running the released `v0.1.0`, built 2026-10-19):

| Time | Event |
|------|-------|
| 08:17:54 | A session of 51497 s (2.5 GB sent) is replaced |
| 08:17:55 | Its replacement is replaced **1.1 s** later, having acked nothing (`ack lag -`) after one full-screen frame |
| 08:18:09 | Replaced again, after 14.5 s |
| 08:18:10 | And again **1.0 s** later, same one-frame-no-ack shape |
| 08:21:16 | The operator restarts the unit; a client reconnects within 1 s |
| 08:25:39–40 | The same pair again: 262 s, then 1.0 s |
| 08:30:23 | Restarted again, this time with a rotated ticket |
| 08:30:24, 08:30:46 | `192.168.1.28` refused: wrong ticket |
| 08:30:43 | A session begins and survives 11 hours |

Two clients each treated a silent drop as a network fault and took
the desktop straight back. The operator stopped it by rotating the
ticket, which locks one client out altogether — the workaround
costs him the client he was trying to use.

`42ef302` ("Tell a replaced client why before dropping it.",
2026-10-25) fixes the fight: the replaced client is sent a
`NOTIFY` naming the address that took over, then `DISCONNECTING`
with reason `SPICE_LINK_ERR_OK`, and ryll's `ServerEnded` state
suppresses every automatic retry that follows. It was never
released — `v0.1.0` is the only release and predates it by six
days — so the dogfood host ran without it until it was upgraded
from a locally built wheel on 2026-11-17.

**The farewell is not enough.** It stops a *displaced* client from
reconnecting. It does nothing about the aggressor: ryll waking from
sleep, recovering from a network blip, or spending its
`MAX_RECONNECT_ATTEMPTS` budget still takes the desktop from
whoever holds it. One-sided, silent theft instead of a war is an
improvement, not a fix. The fix is to let both clients hold the
desktop at once.

### What is already ready for this

The platform seams were built multi-consumer, and this is the
reason the plan is tractable at all:

| Seam | Why N consumers already work |
|------|------------------------------|
| Damage | `ScreenSource::subscribe_damage` gives each subscriber its own pending region, starting as the whole desktop; `DamageHub` holds `Vec<Weak<Shared>>` (`platform/damage.rs`) |
| Capture | `capture` and `capture_all` are stateless reads |
| Cursor | `CursorSource::subscribe` returns a `watch::Receiver` |
| Clipboard | `Clipboard::changes` likewise; `set` is a queue |
| Audio | "Each open is a fresh capture, with a queue of its own" (`platform/mod.rs:712`) |
| Per-session state | The shadow, heat map, H.264 stream, ack window, mouse mode, metrics and session id are already owned per channel or per session |

Nothing in the server is process-global except the registry itself
and the platform handles every task shares.

### What is not ready

- **The registry holds one session.** `begin` replaces; `lookup`,
  `current_id` and `end` are written around a single `current`.
- **Nothing evicts a client that has gone.** `PING` goes out every
  30 s (`main_channel.rs:65`) and the `PONG` is recorded for
  round-trip statistics only (`server/channel.rs:466`). No
  deadline, no eviction. Today replacement is the only garbage
  collector, which is why the journal shows sessions of 11, 14 and
  15 hours ending only when someone else connected. With a bounded
  number of slots, a laptop that sleeps becomes a slot leak.
- **Three resources cannot be shared as they stand.** The desktop
  size (`server/agent.rs:549` turns a monitors config into
  `DisplayControl::set_size`), the keyboard lock state
  (`server/inputs.rs:456`, which merges what the channel holds with
  what the desktop has), and the clipboard.
- **Capture and encode are per session.** Two clients means two
  captures and two encodes of the same pixels; the shadows diverge
  by join time and ack window, so there is no free sharing.
- **The tests assume one session.** `ServerHandle::session()` and
  `session_id()` go through `current_id()` (`server/mod.rs:381`),
  and the replacement tests in `tests/{link,main_channel,inputs,
  display,agent,stream,x11}.rs` assert the one-at-a-time
  behaviour.

### What the two-viewer semantics already are

`docs/xrdp.md:270-295` worked these questions out once already,
for the case of RDP and SPICE showing the same desktop at the same
time, and its answers carry over unchanged: one keyboard layout for
both, because the desktop owns it; lock keys follow whoever
synchronised last; a resize reaches the other viewer as a new
surface. This plan should reuse that reasoning and that page rather
than invent a second set of rules.

## Mission and problem statement

Let a bounded number of SPICE clients hold one X11 desktop at the
same time, so that the operator's laptop and his kasm session can
both be connected, and a client waking from sleep joins the desktop
instead of stealing it.

Specifically:

1. The session registry holds up to `--max-clients` sessions.
2. At capacity, the longest-quiet session is evicted with the
   existing farewell, so `--max-clients 1` is exactly today's
   behaviour.
3. A session whose client has stopped answering pings is evicted
   on a deadline, so a sleeping laptop does not hold a slot.
4. The desktop size, the lock keys and the clipboard have stated
   arbitration rules, consistent with `docs/xrdp.md`.
5. The cost of the second and third client is measured, and the
   duplication of capture and encode is either shared or
   explicitly deferred with numbers behind the decision.
6. The documentation reversal is complete: no page says andris
   serves one client at a time while another says it does not.

Not covered: multi-monitor, migration, per-client views of
different regions or sizes, read-only or view-only clients, any
form of per-client authorisation beyond the single ticket, and
Wayland. A second client authenticates with the same ticket as the
first and has the same powers; anyone who holds the ticket already
has the desktop's keyboard.

## Open questions

These need the operator's answer before the phase they belong to
is planned. Each carries a recommendation.

1. **What should `--max-clients` default to?** *Recommendation: 1.*
   It keeps the released behaviour for everyone who is not asking
   for this, makes the feature opt-in, and means phase 1 can land
   without changing what any existing deployment does. The
   operator sets 2 or 3 in `~/.config/andris/andris.env`.
2. **At capacity, evict or refuse?** *Recommendation: evict the
   longest-quiet session*, with the `42ef302` farewell. It
   composes: at `--max-clients 1` the only session is always the
   longest-quiet one, so the current replace-on-connect behaviour
   falls out of the general rule rather than being a special case.
   Refusing instead would let one half-open session lock a
   legitimate client out until the liveness deadline fires.
3. **How is a resize arbitrated between clients of different
   sizes?** The candidates are last-asker-wins (today's behaviour,
   extended), first-client-owns-the-size, the largest bounding box,
   and refusing resizes whenever more than one client is connected.
   *Recommendation: refuse while more than one client is
   connected*, and say so in the `NOTIFY` andris already sends when
   a resize is refused. Two clients of different window sizes
   otherwise fight over the RandR mode frame by frame, and a
   thrashing desktop is worse for both than a fixed one. This is
   the decision most likely to be argued with, and it is reversible
   in either direction.
4. **Is a shared capture in scope, or deferred on measurement?**
   *Recommendation: deferred until phase 4 has numbers.* One
   session on the dogfood display (2128x1264) costs about 3% of a
   core: the service instance that ran 08:30–20:02 on 2026-11-17
   consumed 21 min 59 s of CPU over 11 h 32 m, and the one before
   it 1 h 25 m over 46 h. If two clients cost 6% there is nothing
   to optimise; sharing the capture while keeping per-client
   shadows is a real piece of work and should not be bought
   speculatively.
5. **Does kerbside need to know?** Each kerbside connection to
   andris would be one more session, and kerbside's own limits on
   concurrent clients per console source are not yet read. Phase 1
   should read them and record the answer, because "two clients"
   through the intended deployment path may mean something
   different from two direct connections.

## Execution

Phases land in andris. Phase plans are written with `/next-phase`
as each phase comes up.

| Phase | Plan | Status | Merged |
|-------|------|--------|--------|
| 1. A registry of N sessions | PLAN-multi-client-phase-01-registry.md | Not started | |
| 2. Liveness and eviction | PLAN-multi-client-phase-02-liveness.md | Not started | |
| 3. Arbitrating the unshareable | PLAN-multi-client-phase-03-arbitration.md | Not started | |
| 4. The cost of a second client | PLAN-multi-client-phase-04-cost.md | Not started | |
| 5. The documentation reversal | PLAN-multi-client-phase-05-docs.md | Not started | |
| 6. Push audit | This file, below | Not started | |

Phases 1, 2 and 3 turn on concurrency and protocol semantics and
should be planned at high effort. Phase 4 is measurement, and
phase 5 follows an established pattern; both can be planned at
medium effort.

### Phase 1: A registry of N sessions

Turn `RegistryState.current` into a map of live sessions, add
`--max-clients` (default per open question 1), and make the
at-capacity policy the eviction of the longest-quiet session
through the existing farewell path. `lookup` searches every live
session, so a secondary channel finds its own; `end` and `close`
iterate.

The session-replacement machinery is kept, not deleted: `replaced`,
`replaced_by`, `secondary_channels_closed` and `FAREWELL_TIMEOUT`
become the at-capacity path rather than the every-connection path.

`ServerHandle` grows a `sessions()` accessor; `session()` and
`session_id()` either stay as "the only session, or panic if there
are several" for the existing tests, or are replaced at their ~65
call sites. The phase plan decides which, and the decision is
worth making deliberately: the first is a smaller diff, the second
leaves no helper that is wrong by construction once the cap is
raised.

Tests cover two sessions coexisting, each with its own channel of
a given type, each seeing damage; a secondary channel naming the
wrong session; and the at-capacity eviction choosing the right
victim. This phase also reads kerbside's per-console client limits
and records the answer to open question 5.

Definition of done includes: at `--max-clients 1`, the existing
test suite passes unchanged.

### Phase 2: Liveness and eviction

Record when each session's main channel last heard a `PONG`, add a
deadline (`--client-timeout`, a small multiple of the 30 s
`PING_INTERVAL`), and end a session that misses it. This is the one
genuinely new mechanism in the plan.

The risk runs one way: evicting a live client on a slow link is
worse than holding a dead slot a little longer, because the dead
slot is merely waste while the eviction is a visible fault. The
phase plan must therefore be explicit about what counts as alive
— a `PONG`, or any message, or an ack — and the default must be
generous.

The eviction is logged with the session, the peer and how long it
had been quiet, and counted in the metrics line that
`main_channel.rs` already emits.

### Phase 3: Arbitrating the unshareable

Decide and implement the three arbitration rules, per open
question 3 and `docs/xrdp.md:270-295`:

- **Desktop size.** Per the recommendation, refused while more than
  one client is connected, with the reason in the existing refusal
  `NOTIFY`.
- **Lock keys.** Already "whoever synchronised last"; the phase
  confirms that N clients do not fight, given ryll repeats its lock
  state on an idle inputs channel
  (`shakenfist/ryll#517`).
- **Clipboard.** The desktop's text is offered to every session;
  a client's text goes to the desktop last-setter-wins. There are
  N agents once there are N main channels, so the phase must check
  that N agents cannot loop text between each other.

Input from several clients interleaves in the platform's queue by
arrival, which is the same thing that already happens between RDP
and SPICE, and is documented rather than changed.

### Phase 4: The cost of a second client

Measure one, two and three clients against the dogfood display,
and compare with the ~3% of a core per client recorded in open
question 4. Add a two-client lane to `scripts/e2e.sh`, which is
also what phase 3's arbitration rules need in order to be tested
end to end.

Then decide whether a shared capture — one capture per frame
feeding N encoders, each keeping its own shadow — is worth
building, and either build it or record the numbers and defer it to
Future work. A deferral with measurements behind it is a result.

### Phase 5: The documentation reversal

Change the seven places in the table above, plus
`docs/running.md`'s connection-limits section, so that none of them
contradicts another. The design commitment becomes a bounded number
of clients, one by default, rather than one client. Add a
`docs/` page on sharing a desktop between clients that states the
arbitration rules in one place. The cross-reference at
`PLAN-x11-desktop.md:125` was added when this plan was created;
what phase 5 still owes is the commitment at
`PLAN-x11-desktop.md:122`, which this plan reverses and which
should say so once the code has landed.

`ARCHITECTURE.md` changes because the shape of the system changed —
the session registry row and the design commitments. `AGENTS.md`
changes only if a convention did.

### Phase 6: Push audit

Run `PUSH-AUDIT.md` over the merge commits recorded in the `Merged`
column for every phase above.

## Agent guidance

### Execution model

All implementation is done by sub-agents briefed from the phase
plans, never in the management session. The management session
plans, reviews the actual diffs and commits.

### Planning effort

This master plan was written at high effort. Phases 1, 2 and 3
turn on concurrency and protocol semantics and should be planned at
high effort; phases 4 and 5 can be planned at medium effort.

### Management session review checklist

- [ ] `pre-commit run --all-files` passes.
- [ ] `make lint` and `make test` pass (cargo runs in Docker,
      never on the host).
- [ ] Andris emits no message type absent from ryll's
      `logging::message_names` tables.
- [ ] No code was translated from x11spice or spice-server.
- [ ] At `--max-clients 1`, behaviour is indistinguishable from
      the behaviour before the phase.

## Administration and logistics

### Success criteria

We will know when this plan has been successfully implemented
because the following statements will be true:

* The code passes `pre-commit run --all-files`.
* `make lint` and `make test` pass. Cargo runs inside Docker,
  never on the host.
* Two ryll clients hold the dogfood desktop at once, each with its
  own cursor, clipboard and sound, and neither displaces the
  other.
* A client that is suspended and never returns is evicted on the
  deadline, and its slot is free.
* At `--max-clients 1`, andris behaves as it did before this plan,
  and the test suite that proved that still passes.
* The cost of each additional client is written down as a measured
  number, not an estimate.
* Every SPICE message type andris sends is named in ryll's
  `logging::message_names`, so kerbside will accept it.
* No code was translated from x11spice or spice-server.
* No page of documentation says andris serves one client at a time
  while another says it does not.

### Documentation index maintenance

This plan has a row in the *Master plans* table of
`docs/plans/index.md`, added when it was created. Each phase plan
is linked from that row as it is written, and the status column
reaches `Complete` when every phase has, including the push audit
and every finding it raises.

### Future work

- **A shared capture path**, if phase 4's measurements say the
  duplication is worth removing and the phase defers it.
- **Per-client views.** Different clients seeing different sizes
  of the desktop would remove the resize arbitration problem
  rather than managing it, and is a much larger piece of work:
  scaling, or a per-client surface.
- **View-only clients.** A second ticket that grants the display
  and cursor channels but not inputs. This is the natural next
  question once several people, rather than one operator's two
  machines, can connect.
- **Volume and mute**, which the audio phase of the X11 desktop
  plan left unforwarded, and which several clients make more
  visible.

### Bugs fixed during this work

To be filed before phase 1 is planned, both found while diagnosing
the 2026-11-17 incident:

- **ryll connects twice per reconnect attempt.** Each attempt in
  the journal arrives as two connections about 1.0–1.1 s apart; the
  first gets one full-screen frame, acks nothing, and is displaced
  by the second. Under one-client-at-a-time this is invisible
  noise. With a cap it is not: a client that takes two slots per
  attempt would fill `--max-clients 2` by itself and lock the
  other machine out. A ryll issue, and a prerequisite for raising
  the cap in anger rather than for phase 1's code.
- **`v0.1.0` predates the farewell.** The only release was built
  2026-10-19 and `42ef302` landed 2026-10-25, so every
  `pipx install andris` has the behaviour that caused the incident.
  A release, or a note in `docs/running.md`, so that the next
  person to install it does not debug what is already fixed.

### Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.
