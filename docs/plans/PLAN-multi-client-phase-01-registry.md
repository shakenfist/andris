# Phase 1: A registry of N sessions

Turn the session registry from one slot into `--max-clients` slots,
and make the at-capacity policy the eviction of the longest-quiet
session through the farewell path that already exists.

Master plan:
[PLAN-multi-client.md](PLAN-multi-client.md), phase 1 of six.

**Planning effort: high.** The master plan assigns high effort to
phases 1, 2 and 3 because they turn on concurrency and protocol
semantics. This phase changes shared mutable state that every
channel task reads.

## Scope

In scope:

- `RegistryState` holds a set of live sessions rather than one, with
  ids unique across every session it knows.
- `--max-clients`, defaulting to 3 (open question 1, answered
  below).
- At capacity, the longest-quiet session is evicted through the
  existing `replaced`/`secondary_closed`/`FAREWELL_TIMEOUT` path.
- A `last_heard` stamp on `Session`, because nothing in the server
  records one today and the eviction rule needs it.
- `ServerHandle::sessions()`, and `session()`/`session_id()`
  redefined as "the only session, or panic".
- The documentation sentences that this phase's default makes
  false.

Out of scope, and why:

- **The liveness deadline** -- phase 2. This phase adds the stamp
  and uses it to choose a victim at capacity; it does not evict
  anyone on a timer, and it does not decide what counts as alive
  for that purpose.
- **Arbitrating the desktop size, lock keys and clipboard** --
  phase 3. Two sessions in this phase can both resize, which is
  today's last-asker-wins extended to N, and is left that way
  deliberately so that phase 3 changes one thing at a time.
- **Sharing capture and encode** -- phase 4, on measurements.
- **The documentation reversal proper** -- phase 5: the new page
  on sharing a desktop, the arbitration rules (which do not exist
  until phase 3), and `ARCHITECTURE.md`'s shape narrative.
- **Fixing ryll's double connect** -- filed as ryll#810 during this
  planning run. It is a prerequisite for *trusting* a raised cap,
  not for landing this phase.

## What the survey found

Phase 1's section in the master plan was written on 2026-11-17,
before the x11-desktop audit's quality fixes landed (#110,
`ae7d03a`). Verified against `develop` at `1a17822`:

Correct as written:

| Claim | Where |
|-------|-------|
| `RegistryState` holds `current: Option<Arc<Session>>` | `server/session.rs:360-367` |
| `begin` swaps the current session | `session.rs:391-418` |
| `lookup`, `end`, `close` are written around one `current` | `session.rs:421`, `:436`, `:451` |
| The replacement machinery to reuse: `replaced`, `replaced_by`, `secondary_closed`, `FAREWELL_TIMEOUT` | `session.rs:152-158`, `mod.rs:727-740` |
| A secondary channel must name the session or be refused | `server/link.rs:267` |
| `--max-clients` does not exist | absent from `src/` and `docs/` |
| The farewell commit | `42ef302`, 2026-10-25 |

Three things the section gets wrong or does not know:

1. **"~65 call sites" for `session()`/`session_id()` is wrong, and
   so is the premise behind it.** There are 32, and **every one is
   in `andris/tests/`**; `andris/src/` has none:

   ```
   $ grep -rhoE '(server|handle|self\.handle|self)\.(session|session_id)\(\)' \
         andris/src --include=*.rs | wc -l
   0
   $ grep -rhoE '(server|handle|self\.handle|self)\.(session|session_id)\(\)' \
         andris/tests --include=*.rs | wc -l
   32
   ```

   The 22 further `conn.session()` calls are `ChannelContext::session()`
   (`server/channel.rs:431`), a different method returning the session
   a channel already belongs to. It stays correct however many
   sessions exist and this phase does not touch it. The master plan
   appears to have counted both methods together.

2. **Nothing records when a session last heard from its client.**
   The master plan says the at-capacity policy is "the eviction of
   the longest-quiet session", and that quantity does not exist. The
   `PONG` handler (`channel.rs:483-499`) records a round-trip
   *duration* into `Timing`, whose `last_us` is the length of the
   last round trip, not when it happened (`stats.rs:117-131`). There
   is no instant to compare. See decision 1.

3. **Session ids are unique only against the one session being
   replaced.** `begin` loops until `id != 0 && Some(id) != previous`
   (`session.rs:392-400`), where `previous` is the single current
   session. With N live sessions a new id can collide with one of
   them, and `lookup` would then hand a secondary channel the wrong
   session. See decision 2.

Minor drift, corrected at source in this phase's first commit: the
master plan's table of the seven places that promise one client
cites `docs/running.md:364`; the sentences are now at `:402-403`,
moved by #110's documentation pass.

### Open question 5, answered during planning

The master plan gave phase 1 the job of reading kerbside's
per-console client limits. Done here instead, because the answer
changes what this phase owes:

**Kerbside imposes no per-console client cap.** Its only cap is
`MAX_CONCURRENT_SESSIONS = 1000`, a coarse global semaphore on
concurrently-handled secure connections
(`rust/kerbside-proxy/src/main.rs:74-79`, `:314-318`). Every
"one client at a time" in kerbside's documentation describes the
*backend* SPICE server's behaviour, not a rule kerbside enforces:
"when a second session connects to a VM, the SPICE server drops the
first session's client" (`docs/console-sources.md:256-258`) and "a
SPICE server serves one client at a time and drops its connected
client when another links" (`docs/proxy-architecture.md:501-504`).

So two clients through kerbside needs no kerbside change: the
property kerbside documents is exactly the one this plan removes
from andris. What it does create is a cross-repository
documentation debt, which belongs to phase 5 rather than here:
`docs/console-sources.md:256` and `:367`,
`docs/proxy-architecture.md:502` and `docs/testing.md:1148` all
state the limit as general to SPICE backends, and will be wrong for
an andris backend.

## Decisions

1. **Add `Session::last_heard`, stamped on any message received on
   any of the session's channels, and evict the session whose stamp
   is oldest.** The quantity the master plan's rule needs does not
   exist (survey finding 2), so this phase creates it. Any message
   is the most generous definition available, which is the direction
   the master plan asks phase 2 to err in; a live-but-idle client
   stays fresh because andris pings it every 30 s and its `PONG`
   counts. Phase 2 still owns the deadline, the eviction timer, and
   narrowing the definition if it has reason to.

   Rejected: **evicting the oldest session by `started`**, which is
   available today and needs no new state. It is backwards for the
   case that motivated the plan. In the 2026-11-17 journal the
   operator's legitimate session was 51,497 s old and the
   double-connect zombie was one second old, so an oldest-first rule
   would evict the client in use and keep the one that renders
   nothing.

2. **A new session id must differ from every session the registry
   knows**, including those still saying goodbye in `replaced`, not
   merely from the one being displaced. A collision would make
   `lookup` ambiguous and could attach a secondary channel to the
   wrong client's session. The loop in `begin` is the only place
   that changes.

3. **Keep `session()` and `session_id()`; add `sessions()`.** Both
   become "the only session, or panic if there is more than one".
   All 32 call sites are test code (survey finding 1), so there is
   no production migration to weigh, and a panic is the behaviour a
   test wants: a test that has unexpectedly grown a second session
   should fail loudly, not silently inspect whichever session the
   iteration order offered. The multi-session tests use
   `sessions()`.

   Rejected: rewriting all 32 call sites. It was the right instinct
   when the accessors looked like production API -- a helper wrong
   by construction is worth removing -- but they are not, and the
   rewrite would make 32 test edits to reach the same place.

4. **The test rig pins `max_clients` to 1; the shipped binary
   defaults to 3.** The operator chose 3 for the binary. The
   existing suite asserts one-at-a-time replacement in
   `tests/{link,main_channel,inputs,display,agent,stream,x11}.rs`,
   and at a default of 3 those tests fail for the right reason: the
   second client no longer displaces the first. So
   `ServerOptions.max_clients` is set to 1 in the rig's single
   construction site (`tests/support/mod.rs:225-232`), a
   `start_with_max_clients(n)` constructor serves the new tests, and
   a CLI test asserts the shipped default is 3 so that the rig's pin
   cannot quietly become the product default.

   **This is the decision most likely to be argued with**, because
   it means the default test configuration is not the shipped
   configuration. The alternative -- running the whole suite at 3 --
   costs rewriting every replacement test in this phase, which is
   phase 3 and phase 5 work pulled forward and done twice. The
   mitigation is that the new tests run at 2 and 3 explicitly, so
   the shipped default is exercised, just not by every test.

5. **This phase makes the six one-client statements true; phase 5
   still owns the reversal.** With the default at 1 the
   documentation could have lagged behind the code until phase 5,
   because nothing changed for anyone who did not opt in. At 3 it
   cannot: the moment this lands, `README.md:34`, `AGENTS.md:12`,
   `docs/index.md:13`, `docs/reading-guide.md:82`,
   `docs/running.md:402-403` and `ARCHITECTURE.md:24` and `:195`
   describe behaviour andris no longer has. This phase makes each
   sentence true in as few words as possible. Phase 5 keeps the
   `docs/` page on sharing a desktop, the arbitration rules, and the
   narrative in `ARCHITECTURE.md`.

6. **Eviction reuses the farewell path untouched.** At capacity the
   victim goes through exactly `Session::replace(peer)`, `replaced`,
   `secondary_closed` and `FAREWELL_TIMEOUT`. So `--max-clients 1`
   is today's behaviour by construction -- the only live session is
   always the longest-quiet one -- rather than a special case
   someone has to remember.

## Step plan

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 1a | high | opus | none | Add `last_heard: Mutex<Instant>` (or an `AtomicU64` of monotonic micros, your call -- say which and why) to `Session` in `andris/src/server/session.rs`, beside `started` at `:146`, with `fn heard(&self)` to stamp it and `fn quiet_for(&self) -> Duration`. Stamp it wherever a message is received for the session: `ChannelConn`'s read path in `andris/src/server/channel.rs`, which already holds the session (`:431`), is the single place that sees every message on every channel -- find the point where a `ReceivedMessage` is returned to the caller and stamp there, not in the per-opcode handlers, so no channel can forget. Do not touch the `PONG` handler's `Timing` recording at `:483-499`; it measures round-trip duration and is unrelated. Unit-test that a message stamps it and that `quiet_for` grows with `tokio::time::advance`. Commit: "Record when a session last heard its client." |
| 1b | high | opus | none | Turn `RegistryState.current: Option<Arc<Session>>` into a collection of live sessions in `andris/src/server/session.rs:360-367`. Prefer `HashMap<u32, Arc<Session>>` keyed by id unless you find a reason not to -- `lookup` is by id and is the hot path. Rewrite `begin` (`:391`), `lookup` (`:421`), `end` (`:436`) and `close` (`:451`) over the set: `lookup` searches all live sessions and keeps its `!is_cancelled()` filter; `end` removes by id; `close` cancels every live session as well as the `replaced` ones it already cancels. `current_id` (`:429`) has no meaning with N sessions -- remove it and give `SessionRegistry` `sessions() -> Vec<Arc<Session>>` and `len()`. The id loop at `:392-400` must reject a candidate matching **any** session the registry knows, live or in `replaced` (decision 2). This step does not add the cap: `begin` still accepts every client, so the suite's replacement tests will fail at the end of this step and pass again after 1c -- say so in the commit message. Commit: "Hold more than one session in the registry." |
| 1c | high | opus | none | Add the cap and the eviction. In `andris/src/server/mod.rs:148`, `ServerOptions` gains `max_clients: NonZeroUsize`; `andris/src/config.rs` gains a `--max-clients` arg following the `--jpeg-quality` pattern at `:158-169` (a `DEFAULT_MAX_CLIENTS: usize = 3` const, `default_value_t`, `value_parser = clap::value_parser!(u16).range(1..)`), documented in its doc comment as the number of clients that may hold the desktop at once; wire it through `andris/src/lib.rs:153`. In `SessionRegistry::begin`, when the live set is already at the cap, choose the session with the largest `quiet_for()` and put it through the existing farewell: `old.replace(peer)`, push it into `state.replaced`, remove it from the live set -- exactly what `:406-416` does today, so read that first and reuse it rather than writing a second path. Log the eviction with the victim's id, peer and quiet time. Unit-test: at a cap of 1 the behaviour is bit-for-bit today's; at 2, a third client evicts the quieter of the two, proven by stamping one of them; and at capacity the evicted session is not the newest. Commit: "Evict the longest-quiet client at capacity." |
| 1d | medium | sonnet | none | Test-facing API. In `andris/src/server/mod.rs`, replace `ServerHandle::session_id` (`:436-439`) and `session` (`:441-447`) with implementations over the new registry: `sessions() -> Vec<Arc<Session>>`, and `session()`/`session_id()` that return the only session and panic with a clear message if `sessions().len() > 1` (decision 3) -- keep their signatures (`Option<...>`) so the 32 existing call sites in `andris/tests/` do not change. In `andris/tests/support/mod.rs`, set `max_clients: 1` in the single `ServerOptions` literal at `:225-232`, and add `start_with_max_clients(n)` beside the other constructors at `:88-158`. `TestServer::session_id` (`:254`) and `wait_for_no_session` (`:259`) keep working unchanged. Do not modify any existing test file. Commit: "Give the tests a handle on several sessions." |
| 1e | high | opus | none | The new tests, in `andris/tests/link.rs` for the registry semantics and a new `andris/tests/multi_client.rs` for the rest (follow `andris/tests/display.rs` for how a display channel is opened and damage awaited). Cover: two clients at `--max-clients 2`, each with its own main and display channel, each receiving its own damage from one desktop change -- the per-subscriber pending region in `platform/damage.rs` should make this work without server changes, and if it does not, that is a finding to report rather than to fix here; a secondary channel naming session B while session A is live attaches to B, and one naming an ended id is still refused with `BadConnectionId` (`server/link.rs:267`); at capacity the quietest session is the one evicted, and it receives the farewell `NOTIFY` naming the new peer before `DISCONNECTING`, as `tests/main_channel.rs` already asserts for replacement; and at `--max-clients 1` a second client replaces the first with the existing behaviour. Commit: "Test two clients on one desktop." |
| 1f | medium | sonnet | none | Make the documentation true, in as few words as each place allows (decision 5): `README.md:34`, `AGENTS.md:12`, `docs/index.md:13`, `docs/reading-guide.md:82`, `docs/running.md:402-403` (its connection-limits section, which also documents the new `--max-clients` option beside the other options), `ARCHITECTURE.md:24` (the session-registry row) and `ARCHITECTURE.md:195` (the design commitment). Each becomes a bounded number of clients, three by default, evicting the longest-quiet at capacity. Do **not** write the arbitration rules for resize, lock keys or the clipboard -- they do not exist until phase 3, and claiming them now is the drift this plan exists to end. Leave `docs/plans/` alone. Verify with the script in this plan's definition of done. Commit: "Say that andris serves more than one client." |

## Risks and mitigations

- **The wrong victim is evicted, and a working client is dropped.**
  This is the failure the plan's own risk paragraph calls visible
  rather than merely wasteful. Mitigation: decision 1's generous
  definition (any message keeps a session fresh), step 1c's test
  that the newest session is never the victim, and step 1e's test
  that the quietest one is. The management session checks the
  stamping site reaches every channel, not just main.
- **An id collision attaches a secondary channel to the wrong
  client's session.** Mitigation: decision 2, enforced in `begin`
  over live and replaced sessions, with a unit test that exhausts a
  deliberately narrowed id space rather than trusting a 32-bit
  random draw to collide.
- **ryll#810 makes the operator's own cap of 3 leak slots.** Each
  reconnect attempt arrives as two connections, so one machine can
  hold two sessions. Whether the leaked one answers pings is
  unknown -- if it does, phase 2's deadline will not reclaim it
  either, because the socket is alive. Mitigation: the cap is 3
  rather than 2, so one leaking client still leaves room for the
  other machine; the eviction rule prefers the quieter session,
  which is the leaked one if it is genuinely idle; and ryll#810 is
  the real fix. This risk is live from the moment this phase lands,
  because the default is 3 rather than 1.
- **Two clients double the capture and encode.** Measured at about
  3% of a core per client in the master plan's open question 4, so
  three clients is about 9%. Phase 4 owns the decision; this phase
  only has to not make it worse. Mitigation: no change to the
  capture path in this phase, which the diff can be checked for.
- **`close()` misses a session and shutdown hangs.** The registry
  now has two collections to drain rather than one. Mitigation: a
  test that shuts down with two live sessions and one still saying
  goodbye, and `STOP_GRACE` bounding it in any case.

## Definition of done

Falsifiable items only.

- `--max-clients` exists, rejects 0, and defaults to 3: a CLI test
  asserts the default, so the rig's pin of 1 cannot become the
  product default unnoticed.
- `make lint` and `make test` pass, and `pre-commit run --all-files`
  is clean. Cargo runs in Docker.
- No existing test file is modified by steps 1a-1d:
  `git diff --numstat develop -- andris/tests/` shows zero deleted
  lines in `link.rs`, `main_channel.rs`, `inputs.rs`, `display.rs`,
  `agent.rs`, `stream.rs` and `x11.rs` at the end of 1d.
- At `--max-clients 1`, the replacement tests pass unmodified, and
  a session that is replaced still receives the `NOTIFY` naming the
  new peer before `DISCONNECTING`.
- At `--max-clients 2`, two clients hold the desktop at once, each
  with its own display channel, and one desktop change reaches
  both.
- At capacity, the evicted session is the one with the largest
  `quiet_for()`, proven by a test in which the victim is chosen by
  stamping rather than by connection order.
- A secondary channel naming a live session that is not the newest
  attaches to that session; one naming an ended id is refused with
  `BadConnectionId`.
- Session ids are unique across live and replaced sessions, proven
  by a test over a narrowed id space.
- No page outside `docs/plans/` says andris serves one client at a
  time:

  ```
  grep -rn 'one client at a time\|one session served at a time' \
      README.md AGENTS.md ARCHITECTURE.md docs/ --include=*.md \
      | grep -v '^docs/plans/'
  ```

  prints nothing.
- `docs/running.md` documents `--max-clients` beside the other
  options, and its connection-limits section states the eviction
  rule.

## Back brief

Before executing any step of this plan, back brief the operator on
your understanding of it and how the work you intend to do aligns
with it.

**Gate before step 1b.** The shape of `RegistryState` -- one
collection or two, keyed by id or not, and whether `replaced` folds
into the same structure -- is cheap to propose and expensive to
redo once four steps have been written against it. Propose the
struct and the signatures of `begin`, `lookup`, `end`, `close` and
`sessions` before editing, and wait.
