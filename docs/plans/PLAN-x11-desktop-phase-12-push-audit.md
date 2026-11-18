# Phase 12: Push audit

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

## Goal

Run `PUSH-AUDIT.md` over the accumulated diff of every phase of this
plan that landed in andris, so that the audit sees what the server,
the platform backends, the encoders, the agent, the video and audio
work and the packaging did to *each other*, rather than what each
phase did on its own. Findings land as their own pull request against
`develop`, recorded in the master plan under *Items deferred from the
push audit*. The plan is not complete until every finding is fixed or
declined in writing.

## Planning effort

**High.** The master plan's planning-effort section did not mention
phase 12 at all; it has been corrected at source to say high, with
the reason.

The reason is size. The accumulated diff is **63,239 insertions
across 149 distinct files in 16 landings** -- very nearly four times
`PLAN-stream-caps-and-flap` phase 18 in ryll (16,159 insertions),
which is the largest closing audit this fleet has run. A wrapper plan
that hands one agent 63,000 lines and asks for findings gets a skim,
and a skim that reports nothing is exactly the failure the push-audit
phase exists to prevent. Deriving the range, splitting the patch,
and settling what the runbook's own stale file-size rule means for
the audit is the work, and it has been done here so the steps stay
light.

## Scope

In scope:

- **Bringing `PUSH-AUDIT.md` current before the audit runs**: the
  `source-file-size` shared block is a version behind, and v2
  changes what the auditor does rather than merely how it is worded
  (see the survey). This closes issue #85.
- **Deriving and recording the audit range** for the 16 andris
  landings, including the empty-tree form that phase 0 needs and
  that the runbook's recipe cannot express.
- **Both waves of `PUSH-AUDIT.md`** against that range.
- **The wave 2 judgment reviews, split by area** (decision 3).
- **Triaging every finding against current `develop`**, since the
  oldest part of the range landed on 2026-10-08 and the newest on
  2026-10-28.
- **Recording each finding as fixed or declined, in writing**, in
  the master plan.
- **Auditing the master plan's success criteria** against evidence,
  and setting the plan's status from what that finds (decision 8).

Out of scope:

- **Fixing the findings.** Fixes land as their own pull request
  against `develop`, per the shared block. This phase's pull request
  is the plan, the runbook fix and the master-plan corrections.
- **The ryll landings.** Phases 2, 3, 7, 8 and 9 each landed work in
  ryll as well as here. Those commits were audited in ryll, as part
  of the pull requests that landed them, against ryll's own
  `PUSH-AUDIT.md` and blast radius. This phase cites those audits
  and does not repeat them; re-auditing another repository's code
  against andris's runbook would be the wrong question asked
  expensively.
- **The whole-file human review.** Phase 11 installed the tooling and
  the reading moved out of this plan on 2026-11-18. An unread file is
  not a push-audit finding.
- **Adopting ryll's `tools/audit/` harness** (decision 4), or
  development's `source-size-ratchet` template. Both are real
  candidates; neither is this phase's work.

## What the survey found

The master plan's phase 12 section is four sentences written on
2026-10-08, before any phase had landed. One of its two factual
claims is wrong, and the runbook it points at has a gap and a stale
rule. Both wrong claims have been corrected at their source in the
master plan as part of this phase's planning commit, so a later step
need not redo it.

### The master plan is wrong about which phases landed in ryll

The phase 12 section says "For phases 2 and 3, cite the audits run
in ryll's pull requests", and the Execution table's preamble says
"Phases 2 and 3 land in ryll; the rest land here." Phases **7, 8 and
9** each landed ryll work too, as the `Merged` column itself records:

| Phase | ryll landing |
|-------|--------------|
| 2 | `3763158` (ryll #477) |
| 3 | `cd2fe41` (ryll #487) |
| 7 | `b298b39` (ryll #534) |
| 8 | `a157ced` (#552), `601b9dc` (#554), `49072fc` (#557) |
| 9 | `38c51ef` (#578) |

Taken literally, the section would have the audit re-read five ryll
commits under andris's runbook while citing only two. Corrected at
source; the audit cites all five phases' ryll work.

### The range is exact, and the `Merged` column is complete

Unlike ryll's phase 18, no archaeology is needed: every one of the
16 andris landings is recorded, and all 16 are ancestors of
`develop` at `76e444c`. Fifteen are merge commits with two parents,
so `git diff <m>^1 <m>` is the whole of each.

### Phase 0's range cannot be expressed the way the runbook says

Phase 0 is recorded as the direct range `60c0c4d..e89539f`, and
`PUSH-AUDIT.md` says to "append `git diff first^ last`" for a range.
`60c0c4d` is the **repository's root commit**. It has no parent, so
`60c0c4d^` is not a revision and the recipe fails:

```
$ git diff 60c0c4d^ e89539f
fatal: ambiguous argument '60c0c4d^': unknown revision or path not
in the working tree.
```

The working form diffs against the empty tree, which is correct here
because phase 0 *is* the start of the repository -- there is no prior
state to exclude:

```
git diff "$(git hash-object -t tree /dev/null)" e89539f
```

This is a gap in the runbook rather than in the plan, and it is a
gap only a repository whose first master plan starts at its root
commit can hit, which is why no previous audit met it. Step 12a adds
the one sentence to `PUSH-AUDIT.md`. The same prose appears in
ryll's and other repositories' runbooks by convention, so it is
worth raising with shakenfist/development; that is a follow-up for
the operator to decide on, not this phase's work.

### The diff is large, and a quarter of it is plan files

| Slice | Paths | Insertions | Deletions |
|---|---|---|---|
| `andris/src/server/**` | 46 | 13,012 | 720 |
| `andris/src/platform/**` | 47 | 12,120 | 318 |
| `andris/src/*` (config, install, lib, main) | 15 | 2,382 | 95 |
| `andris/tests/**` | 30 | 8,361 | 529 |
| Build, CI, scripts, packaging, manifests | 119 | 12,039 | 371 |
| Documentation (`*.md` outside `docs/plans/`) | 61 | 4,889 | 310 |
| `docs/plans/**` | 47 | 10,436 | 217 |
| **Total** | **365** | **63,239** | **2,560** |

The `Paths` column counts diff entries across the 16 landings, so a
file touched by six phases counts six times; the distinct-path count
is 149. The slices partition the diff exactly -- the insertions and
deletions sum to the total, and so do the rows -- which is step
12b's coverage proof.

The largest single landing is phase 4's server skeleton (`1950cdb`,
16,391 insertions); the smallest is the 0.1.0 release bump
(`c0617e9`, 2 insertions).

### The runbook's file-size rule is a version behind

Issue #85 reports `PUSH-AUDIT.md`'s `source-file-size` shared block
as v1 where development's `main` carries v2. It is the only stale
block of the ten embedded; the other nine match.

This is not cosmetic. v2 adds two things that change what wave 2a
does:

- **"The push auditor reads the files the diff touches, not only the
  diff."** A branch that grows a file already over 800 lines is a
  finding even when its own addition is small, "because per-diff
  review is how slow growth gets past: every step looks reasonable
  on its own." That is precisely the shape of this audit -- twelve
  phases each adding a few hundred lines to the same modules.
- **The ratchet rule**: no file may cross 1,500 lines, and no file
  already over it may grow, without a reviewed edit to the
  repository's baseline of oversize files. Andris has **not**
  adopted development's `source-size-ratchet` template (nothing in
  `.pre-commit-config.yaml`, `tools/`, the `Makefile` or CI
  references it), and v2 says that where a repository has not, "the
  push auditor applies the same rule by hand".

Auditing 63,000 insertions against the pre-ratchet wording would
apply the wrong criterion to the one dimension where this diff is
most likely to have a real finding. Hence decision 2.

### Nineteen files are already over the threshold

Measured on `develop` at `76e444c`, with no generated or vendored
file among them (none carries a generation banner):

| File | Lines |
|---|---|
| `andris/src/server/updates/stream.rs` | 1,681 |
| `andris/src/platform/x11/clipboard.rs` | 1,362 |
| `andris/src/platform/synthetic/mod.rs` | 1,309 |
| `andris/src/platform/damage.rs` | 1,296 |
| `scripts/e2e-drive.py` | 1,241 |
| `andris/src/server/display.rs` | 1,189 |
| `andris/tests/stream.rs` | 1,110 |
| `andris/src/config.rs` | 1,097 |
| `andris/src/platform/x11/randr.rs` | 1,061 |
| `scripts/e2e-xrdp-drive.py` | 1,044 |
| `andris/src/server/agent.rs` | 1,023 |
| `andris/src/platform/mod.rs` | 989 |
| `andris/tests/x11.rs` | 950 |
| `andris/src/install.rs` | 949 |
| `andris/src/server/channel.rs` | 920 |
| `andris/src/platform/x11/mod.rs` | 919 |
| `andris/tests/agent.rs` | 911 |
| `andris/tests/display.rs` | 887 |
| `andris/src/server/stats.rs` | 845 |

One file is over 1,500. Under the v2 rule that is the one wanting a
stated reason to stay whole, and the other eighteen each open a
question rather than record a defect. Issue #15 already covers
`clipboard.rs`, so that one is tracked rather than new. Stating this
list in the plan is deliberate: it is the finding set a reader can
check the audit against, and an audit that comes back without
mentioning `stream.rs` has not looked.

### `unsafe` is narrow, and `unwrap` is linted

`andris/src/lib.rs` carries `#![deny(unsafe_code)]` crate-wide, with
exactly three `allow` sites: `platform/pulse/ffi.rs` (module-wide,
for the libpulse FFI), `platform/qemu/mod.rs` and
`platform/x11/mod.rs`. The workspace sets `clippy::unwrap_used =
"warn"` with `allow-unwrap-in-tests = true`. Three TODO/FIXME/HACK
comments exist across `andris/src` and `scripts`. So wave 2a's
mechanical scan has a small, known target surface, and the judgment
part is where the value is.

### There is no audit harness here, and ryll's is not a template

Ryll automates both waves in `tools/audit/` -- 1,546 lines across
seven files, with their own pre-commit-gated tests. Andris has
nothing equivalent, and `PUSH-AUDIT.md` here describes the manual
commands instead. The harness is not one of development's
templates, so no audit asks andris for it. Decision 4 declines to
port it.

### Wave 1 should pass

`develop` at `76e444c` is green: the last three days of workflow
runs on it are scheduled jobs only (Renovate, prune-reviews, the
config export), all successful, and nothing has landed since
`76e444c` except that commit itself. Wave 1 still runs for real in
step 12c -- it runs against the current tree, not the range, so a
failure there would be a regression on `develop` today rather than
a finding against this plan, and the step says to report which.

## Decisions

1. **Audit the diff as it landed, then triage every finding against
   current `develop` before acting.** The same call ryll's two
   closing audits made, and the one a reviewer is most likely to
   argue with: auditing today's tree would produce findings that are
   all immediately actionable, and would be cheaper. It is rejected
   because this phase exists to ask what *this plan* did to the
   codebase. Code that later work has since refactored has not
   thereby been audited, and a closing audit that reads only the
   current tree cannot see what phase two made wrong and phase five
   never revisited -- which is the stated reason the phase audits an
   accumulated diff at all. The cost is step 12f, and it is bounded:
   the newest landing is three weeks old and the oldest six, so
   drift is far less than the three months ryll's phase 18 had to
   triage across.

2. **Fix the runbook before running it, inside this phase.** Issue
   #85's stale `source-file-size` block and the root-commit gap are
   both corrected in step 12a, as the phase's first commit. The
   alternative -- a separate `/consistency-fix` pull request, with
   this phase waiting on it -- is the tidier process answer and is
   rejected because the audit cannot be run correctly in the
   meantime: v2 tells the auditor to read whole files rather than
   only the diff, and to apply the 1,500-line ratchet by hand where
   the repository has no ratchet, which andris does not. Running
   wave 2a against v1 would apply the superseded rule to the single
   dimension where this diff is most likely to have something real.
   The fix is a verbatim block copy and a one-sentence addition, it
   closes an open consistency issue, and it is directly in service
   of the phase. This is the narrowest exception to "the plan's job
   is to record a gap, not fix it" that gets the audit right.

3. **Six judgment patches, split by area; no agent gets the whole
   63,000 lines.** The runbook's wave 2 briefs assume a diff an
   agent can hold in mind. Code quality and security each split
   across the two halves where the risk differs in kind -- the
   server (wire readers, authentication, flow control) and the
   platform backends (X11 capture and injection, clipboard, RandR,
   the libpulse FFI, shared memory). Test review and documentation
   review stay whole, because both ask cross-cutting questions that
   a split makes harder rather than easier: whether every wire type
   andris writes is round-tripped, and whether any one fact is
   stated two different ways on two pages. Six agents, not four and
   not twelve.

4. **Do not port ryll's `tools/audit/` harness.** It would be 1,546
   lines of scripts plus their tests imported into andris, and it
   would change `PUSH-AUDIT.md` -- the file this phase is trying to
   run -- from a manual runbook into a scripted one. What it
   automates is the mechanical scanning, which is a handful of greps
   over a patch file; what is expensive here is the judgment, which
   no harness helps with. The scripts are not ryll-specific
   (`audit-range.sh`, `wave1-checks.sh` and `wave2-mechanical.sh`
   contain no reference to ryll or its crates), so templatising them
   in shakenfist/development is a genuinely good idea and a poor
   thing to attempt inside a closing audit. Recorded as future work
   in the master plan; not done here.

5. **Security review gets the largest share, and it is split
   server/platform because the threat differs.** Andris is a network
   server that drives the operator's real desktop, so it has two
   distinct attack surfaces and one agent asked about both will do
   neither well. The server half (`andris/src/server/**`, 13,012
   insertions) parses untrusted client bytes -- the channel readers,
   the ticket authentication, TLS, the display and inputs and agent
   message paths, and the flow control that a slow or hostile client
   can stall. The platform half (`andris/src/platform/**`, 12,120
   insertions) is where a client's bytes become effects on a real X
   session: XTEST input injection into the operator's desktop, the
   clipboard in both directions, RandR, and the three `allow(
   unsafe_code)` sites. Both get opus at high effort.

6. **`docs/plans/**` is excluded from the judgment patches.** It is
   10,436 insertions, a sixth of the diff, and it is this plan
   describing itself. A documentation agent handed it would spend
   most of its budget reading planning prose for consistency
   findings that do not matter. The documentation agent instead gets
   the 4,889-insertion documentation slice and checks the
   plan-reference invariants -- that no document outside
   `docs/plans/` cites a phase number, and no code does either --
   against the other slices, which is the part of the plan files
   that can actually be wrong somewhere a reader will meet it.
   Counted in the coverage proof so that the exclusion is deliberate
   and visible rather than an accident.

7. **Findings land as their own pull request, separate from this
   plan.** This phase's pull request is the plan, the runbook fix and
   the master-plan corrections. Keeping the fixes out means a review
   of the audit is a review of the audit, and it is what the
   push-audit shared block asks for.

8. **This phase does not assume the master plan goes `Complete`.**
   The plan has eight success criteria. Two are annotated as met or
   partly met in phase 10; the rest -- the 2% idle CPU figure, 30
   frames per second while scrolling, clipboard in both directions,
   the end-to-end CI lanes, `docs/` describing xrdp coexistence
   without plan references, and the consistency audit filing no
   issues other than `review-coverage` -- carry no evidence in the
   plan one way or the other. Step 12h reads each against evidence
   and records a verdict. The status moves to `Complete` only if
   every criterion is met and every finding is resolved or declined;
   otherwise it stays `In progress` with the reason named, which is
   what the index's own vocabulary section asks for. A closing audit
   is exactly the moment unmet criteria get quietly lost, so this is
   a step rather than an assumption.

## Step plan

The management session reviews each diff and makes each commit. No
sub-agent commits, pushes, or edits `.vscode/*.weaudit*`. Run every
make target as `env -u DISPLAY -u XAUTHORITY -u PULSE_SERVER -u
PULSE_COOKIE make ...`: no sub-agent connects to the host's `:10`
session or its sound server. Cargo runs only inside the devcontainer,
never on the host.

Steps 12a and 12b are independent of each other. Steps 12c and 12d
need 12a (the runbook) and 12b (the patches). The six agents of 12e
run in parallel. 12f needs all of them.

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 12a | low | sonnet | none | Bring `PUSH-AUDIT.md` current, in its own commit, subject "Update the push audit's file-size rule." Two edits. (i) Replace the `source-file-size` shared block -- everything from `<!-- shared-block: source-file-size v1 -->` to the matching `<!-- shared-block-end -->` inclusive -- with development's current copy, **byte for byte**. Get it from a fresh clone of `https://github.com/shakenfist/development` on `main` in a scratch directory; do not read or write the operator's checkout at `../development`, which is theirs and may be behind. Verify with `git hash-object` that the extracted block matches `templates/shared-blocks/source-file-size.md` in that clone, and print both hashes. The current version is v2; if the clone shows something later, copy what it shows and say so. (ii) In the *Two ways this runbook is invoked* section, after the sentence beginning "For a phase recorded as a `first..last` range", add that where `first` is the repository's root commit it has no parent, so `first^` is not a revision, and the diff is taken against the empty tree instead: `git diff "$(git hash-object -t tree /dev/null)" last`. Wrap at the file's existing width. Change nothing else in the file -- in particular do not reword the other nine blocks, which are current. `pre-commit run --all-files` must pass. Mention issue #85 in the commit body; the pull request closes it. |
| 12b | low | haiku | none | Assemble the audit patches into a scratch directory and print their diffstats. Nothing in this step interprets anything. The 16 landings are the 15 merge commits `cf34d91 a9e79cf 591e062 1950cdb 68b1058 43bfe88 498a72c b7ba2dd c2fe8a7 1ecafa5 d8f316a 553ecfb c0617e9 c36e87a 568a43b`, each contributing `git diff <m>^1 <m>`, plus phase 0, which contributes `git diff "$(git hash-object -t tree /dev/null)" e89539f` because `60c0c4d` is the root commit and has no parent. Concatenate all 16 into `whole.patch`. Then build six area patches over the same 16 landings, each with a `-- <pathspec>` filter: `server.patch` (`andris/src/server`), `platform.patch` (`andris/src/platform`), `core.patch` (`andris/src ':(exclude)andris/src/server' ':(exclude)andris/src/platform'`), `tests.patch` (`andris/tests`), `harness.patch` (`scripts .github Makefile packaging .devcontainer tools .cargo Cargo.toml Cargo.lock clippy.toml deny.toml renovate.json .gitignore .vscode .skillsaw.yaml .pre-commit-config.yaml LICENSE andris/Cargo.toml andris/pyproject.toml andris/build.rs`) and `docs.patch` (`':(glob)*.md' ':(glob)docs/*.md' ':(exclude)docs/plans'`). Also measure, but do not write, the `docs/plans` slice. **The gate, and the point of this step:** the seven slices must partition the whole diff exactly. Print, from `--numstat` over the same landings, each slice's row count, insertions and deletions, and check that they sum to the whole. Expected: 365 rows, 63,239 insertions, 2,560 deletions in total, 149 distinct paths; server 46/13,012/720; platform 47/12,120/318; core 15/2,382/95; tests 30/8,361/529; harness 119/12,039/371; docs 61/4,889/310; plans 47/10,436/217. If any number differs, stop and report: the range or the pathspecs broke, and everything after this is wasted. |
| 12c | low | sonnet | none | Wave 1 of `PUSH-AUDIT.md`, from the repository root on `develop` at `76e444c`: `pre-commit run --all-files`, then `env -u DISPLAY -u XAUTHORITY -u PULSE_SERVER -u PULSE_COOKIE make lint`, then the same for `make test`. These run against the **current tree**, not the audit range, which is what the runbook intends; so a failure is a regression on `develop` today rather than a finding against this plan, and the report must say which it is. Report each command's exit status and the full output of any failure. Do not fix anything. If any of the three fails, stop and report rather than going on: wave 2 is only worth spending on if wave 1 passes. Note that `make lint` and `make test` run cargo inside the devcontainer, which may need to build the image first; allow for that and do not run cargo on the host. |
| 12d | low | sonnet | none | Wave 2's mechanical scan plus the *Style conformance -- judgment portion* brief from `PUSH-AUDIT.md`, against `whole.patch` from step 12b rather than a revision range. Mechanical, over added lines only: TODO, FIXME, HACK and XXX; new `#[allow(dead_code)]`; new `unsafe` blocks and new `#[allow(unsafe_code)]`; new `.unwrap()` and `.expect()` outside `#[cfg(test)]`. The expected shape, from the survey, is small: three TODO-family comments in `andris/src` and `scripts` on current `develop`, and exactly three `allow(unsafe_code)` sites -- `platform/pulse/ffi.rs` module-wide, `platform/qemu/mod.rs` and `platform/x11/mod.rs` -- against a crate-wide `#![deny(unsafe_code)]`. A scan that finds many more has matched test code or comments; say so rather than reporting the count. Then the style-conformance judgment part, verbatim from the runbook, with its four andris-specific invariants: every SPICE message type andris sends is named in ryll's `logging::message_names` tables (a type ryll does not model is **blocking** -- kerbside terminates the session on it); nothing depends on `shakenfist-spice-renderer`; platform concerns sit behind their traits rather than being called from protocol code; and no field silently changed units without a rename or a doc comment. Report, do not fix. |
| 12e | high | sonnet for 2a-1, 2a-2, 2b, 2c; opus for 2d-1, 2d-2 | none | Six judgment agents from `PUSH-AUDIT.md`'s wave 2, run in parallel, each given a **patch file path** from step 12b rather than a revision range, and each using the runbook's brief verbatim plus the additions below. All six: the patches are the accumulated diff of phases 0 to 11 as they landed between 2026-10-08 and 2026-10-28; report what the patch shows and do **not** check it against the current tree -- that is step 12f's job, and six agents redundantly doing it is the waste this split exists to avoid. All six: the `source-file-size` block in the runbook is the one at `HEAD` after step 12a, which is v2. **2a-1** (code quality, `server.patch`): twelve phases layered onto the same modules, so the duplication to look for is across channel handlers -- `channel.rs`, `display.rs`, `inputs.rs`, `agent.rs`, `playback.rs`, `session.rs` and `updates/` -- and wire handling that belongs in ryll's `shakenfist-spice-protocol` or `shakenfist-spice-compression` rather than here. Under v2's rule, read the whole of each file the patch touches, not only the diff: `updates/stream.rs` is 1,681 lines and over the 1,500 line that wants a stated reason, and `display.rs` (1,189), `agent.rs` (1,023), `channel.rs` (920) and `stats.rs` (845) are over 800. Say for each whether the length has a seam to split along. **2a-2** (code quality, `platform.patch` + `core.patch`): the X11, synthetic and QEMU backends implement the same traits, so look for logic that should sit in `platform/mod.rs` and is instead repeated per backend, and for the synthetic backend drifting from what the real one does. Whole-file reading again: `clipboard.rs` (1,362 -- issue #15 already proposes splitting it, so confirm or correct that rather than re-filing), `synthetic/mod.rs` (1,309), `damage.rs` (1,296), `randr.rs` (1,061), `platform/mod.rs` (989), `config.rs` (1,097) and `install.rs` (949). **2b** (test review, `tests.patch` + `whole.patch` for context): the runbook's question that matters most here is whether everything andris writes to the wire is round-trip tested against the parser ryll's client actually uses. Enumerate the message types the server sends and say which are round-tripped, which are asserted only on bytes, and which are untested. Also: four test files are over 800 lines (`stream.rs` 1,110, `x11.rs` 950, `agent.rs` 911, `display.rs` 887); say whether they have grown into integration suites that want splitting. Distinguish "untested" from "untestable on CI" -- the X11 and PulseAudio paths differ there -- because they are different findings. **2c** (documentation review, `docs.patch` plus `harness.patch` for the build and CI facts): the plan-reference invariants are the ones to check hardest, since this repository has just finished eleven phases -- grep the documentation slice and the code slices for "phase <number>" and for `docs/plans` or `PLAN-` citations outside `docs/plans/`, and report each as a finding with its line. Also check that `ARCHITECTURE.md` and `AGENTS.md` have not grown content belonging in `docs/`, that `README.md` is still a pitch, and that no document describes planned behaviour as if it exists. `docs/plans/**` is deliberately not in your patch (see decision 6). **2d-1** (security, opus, high effort, `server.patch`): the untrusted-input surface. Every byte a client sends is attacker-controlled. Check the channel readers for unchecked indexing, allocation sized from client-supplied lengths, and overflow in size arithmetic; the ticket path for a file read whose permissions are not checked, a ticket that reaches a log, a comparison that leaks timing, and any path that reaches a channel before authentication succeeds; the listener for a non-loopback bind that can run without TLS, and for certificate and key handling; the flow control and stream machinery for unbounded memory, descriptor leaks or CPU spin driven by a slow or hostile client; and the new `Arc`/`Mutex`/atomic patterns for deadlock and race. **2d-2** (security, opus, high effort, `platform.patch` + `core.patch`): where client bytes become effects on the operator's real desktop. XTEST input injection -- can a client reach input the SPICE session should not grant, or the X server's credentials? The clipboard in both directions, including what leaves the desktop unasked (issue #14 covers password-manager hints; confirm the surface rather than re-filing). RandR geometry from client-supplied monitor configuration. The three `allow(unsafe_code)` sites -- `platform/pulse/ffi.rs`, `platform/qemu/mod.rs`, `platform/x11/mod.rs` -- for documented and sound invariants, correct `Send`/`Sync` claims on FFI handles, and lifetime of shared-memory segments in `x11/shm.rs` and `qemu/shm.rs`. `install.rs` writes a systemd unit: check the paths it builds from configuration. |
| 12f | high | opus | none | Triage. Take every finding from 12c, 12d and 12e and classify each against **current `develop`** as `still-present`, `already-fixed` or `moved`. All audited files are still at the same paths, and the drift is small -- the newest landing is three weeks old, and the only things that have touched `develop` since are Renovate bumps, two review-tracking template re-syncs (#65, #66), one consistency-fix pull request (#71) and the prune bot. So assume a finding is live unless you can point at what fixed it: `already-fixed` **requires** naming the commit or the current code that resolves it, and a finding you cannot locate stays `still-present` for a human to look at. For each `still-present` finding give the current file and line. Check each against the open issue list before reporting it as new -- #13 (adversarial agent tests), #14 (clipboard password-manager hints), #15 (clipboard module split), #25 (waiting for a local X display) and #62 (D-Bus spike deferred work) already exist, and a finding that restates one of those should say so. Output one table: finding, source agent, severity, status, current location, existing issue if any. |
| 12g | low | sonnet | none | Record the derivation in the master plan so it never has to be done again, in its own commit, subject "Record the push audit's range derivation." The `Merged` column is already complete, so this step adds to the phase 12 section the 16 landings as the audit range, the empty-tree form phase 0 needs and why, and the seven-slice partition with its numbers. Put commits and ranges in that section, not in the `Status` column, which holds one vocabulary term. Do not restate the findings here; they go in step 12h's section. |
| 12h | medium | management | none | Management step, not a sub-agent step. Three parts. (i) Review 12f's table, decide fix-or-decline for every finding, and record the outcome in the master plan under a new *Items deferred from the push audit* heading, with a reason in writing for each declination. If the audit found nothing, that is one sentence and a real result. (ii) Fill in the master plan's *Bugs fixed during this work* section, which currently names two ryll bugs from phase 2 and nothing since -- twelve phases either fixed things or did not, and both are answers. (iii) Work through the eight success criteria one at a time and record a verdict with evidence for each, per decision 8. Then set the master plan's status and its `docs/plans/index.md` row: `Complete` only if every criterion is met and every finding is resolved or declined; otherwise `In progress` with the reason named in the plan. Note that the consistency criterion now reads "files no issues other than `review-coverage`", and that #85 is closed by this phase's own pull request. Fixes land as their own pull request; this step decides and records. |

## Risks and mitigations

- **The audit reports "no findings" because it looked at nothing.**
  The failure the phase exists to prevent. On a closing audit the
  branch diff is empty, so a range that silently breaks produces a
  clean report. *Mitigation:* step 12b is a gate with the expected
  numbers written into the brief and into this plan -- 365 rows,
  63,239 insertions, 2,560 deletions, 149 paths. A reviewer should
  check those four numbers before reading anything else. If they do
  not match, nothing downstream is worth reading.
- **An agent skims because its slice is still too big.** The two
  largest slices are 13,012 and 12,120 insertions, which is still
  more than ryll's phase 18 gave any single agent. *Mitigation:*
  each brief names the specific files and the specific hazard
  classes rather than asking for "findings", and the survey states
  the expected file-size finding set. If 2a-1 comes back without
  mentioning `updates/stream.rs`, or 2d-2 without mentioning the
  three `unsafe` sites, re-run that agent per-module rather than
  accepting the result.
- **The file-size findings swamp everything else.** Nineteen files
  are over 800 lines, and v2's whole-file rule invites a finding for
  each. Nineteen advisory findings would bury a real security one.
  *Mitigation:* the briefs ask for a seam-or-not judgment per file
  rather than a line count, the survey lists them in the plan so the
  agents are not reporting news, and step 12h groups them as one
  item with one decision rather than nineteen.
- **Fixing the runbook inside the phase grows into a consistency
  sweep.** Step 12a touches the file the audit runs, and the
  temptation is to tidy the other nine blocks while there.
  *Mitigation:* the survey verified that the other nine are current,
  the brief says to change nothing else, and the step is its own
  commit so the diff is reviewable at a glance.
- **The phase quietly closes a plan that is not closed.** Six of
  eight success criteria carry no evidence either way.
  *Mitigation:* decision 8 and step 12h(iii), and a definition-of-done
  item that asserts each criterion has a verdict with evidence
  rather than a tick.
- **The ryll work goes unaudited by anybody.** Five phases landed
  code in ryll, and this phase cites rather than reads it. If those
  pull requests did not actually run an audit, the citation is
  empty. *Mitigation:* step 12h records, for each of the five, which
  ryll pull request carried the audit, so the citation is checkable.
  Where one did not, say so plainly rather than implying coverage.

## Definition of done

Falsifiable items only.

- `PUSH-AUDIT.md`'s `source-file-size` block is byte-identical to
  development `main`'s `templates/shared-blocks/source-file-size.md`.
  This prints `ok`:

  ```
  python3 -I - <<'EOF'
  import re, subprocess, sys, tempfile, pathlib
  d = tempfile.mkdtemp()
  subprocess.run(['git', 'clone', '--depth', '1', '-q',
                  'https://github.com/shakenfist/development', d],
                 check=True)
  want = pathlib.Path(d, 'templates/shared-blocks/source-file-size.md').read_text()
  have = open('PUSH-AUDIT.md').read()
  m = re.search(r'<!-- shared-block: source-file-size v\d+ -->.*?<!-- shared-block-end -->\n',
                have, re.S)
  assert m, 'block not found'
  assert m.group(0) == want, 'block differs from development main'
  print('ok')
  EOF
  ```

- `git diff 60c0c4d^ e89539f` still fails, and `PUSH-AUDIT.md` says
  what to do instead: `grep -c 'hash-object -t tree /dev/null'
  PUSH-AUDIT.md` is at least 1.
- Issue #85 is closed by this phase's pull request.
- The assembled `whole.patch` matches **365 diff rows, 63,239
  insertions, 2,560 deletions** over 149 distinct paths, and the
  seven slices' rows, insertions and deletions each sum to those
  totals. Step 12b's output recording this is pasted into this
  file's Result.
- Wave 1's three commands have each been run and their exit statuses
  are recorded in this file. Any failure is labelled as a regression
  on `develop` or as a finding against the plan.
- All six wave 2 judgment agents have reported, and each report is
  either summarised in this file or its findings appear in 12f's
  table.
- 12f's table exists; every row carries `still-present`,
  `already-fixed` or `moved`; every `already-fixed` row names what
  fixed it; and every row that restates an existing issue names it.
- The master plan's phase 12 section records the 16 landings, the
  empty-tree form for phase 0, and the seven-slice partition.
- The master plan has an *Items deferred from the push audit*
  section in which every finding is marked fixed or declined with a
  reason -- or one sentence recording that the audit found nothing.
- The master plan's *Bugs fixed during this work* section says
  something about phases 4 to 11, not only about phase 2's two ryll
  bugs.
- Each of the master plan's eight success criteria carries a verdict
  with evidence.
- The master plan's phase 12 row reads `Complete`. The plan's own
  status, in both the master plan and `docs/plans/index.md`, reads
  one vocabulary term, the same term in both, and if it is not
  `Complete` the plan names what is outstanding.
- For each of phases 2, 3, 7, 8 and 9, the plan names the ryll pull
  request whose audit is being cited, or says that none was run.
- `pre-commit run --all-files` passes, and `make lint` and `make
  test` pass.

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.

**Gate after step 12b.** The patch partition is the whole basis of
the audit, and it is cheap to check and expensive to redo: six
agents run against those slices, and a slice that quietly dropped
2,000 lines means re-running them. Show the operator step 12b's
diffstat table against the expected numbers before any agent of 12e
starts.

**Gate before step 12h sets the status.** Whether this plan is
`Complete` is the operator's call, not an audit's. Bring them the
eight criteria with verdicts and let them decide.

## Result

Not started.
