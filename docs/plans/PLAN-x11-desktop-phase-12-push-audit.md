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
"warn"` with `allow-unwrap-in-tests = true`, and `andris/Cargo.toml`
inherits it with `[lints] workspace = true`, so `make lint`'s
`-D warnings` makes a production `unwrap()` a build failure. There
are **no** TODO, FIXME, HACK or XXX comments in `andris/src` or
`scripts`: a grep appears to find three, but all three are
`mktemp ... XXXXXX` templates, and this survey originally miscounted
them as comments. So wave 2a's mechanical scan has a small, known
target surface, and the judgment part is where the value is.

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
   The plan has nine success criteria. Three carry an annotation --
   the soak partly met in phase 10, the video stream met in phase
   10, and the review-tracking adoption met in phase 11. The other
   six -- the 2% idle CPU figure, 30 frames per second while
   scrolling, clipboard in both directions, the end-to-end CI lanes,
   `docs/` describing xrdp coexistence without plan references, and
   the consistency audit filing no issues other than
   `review-coverage` -- carry no evidence in the plan one way or the
   other. Step 12h reads each against evidence
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
| 12f | high | management (planned: opus) | none | Triage. Take every finding from 12c, 12d and 12e and classify each against **current `develop`** as `still-present`, `already-fixed` or `moved`. All audited files are still at the same paths, and the drift is small -- the newest landing is three weeks old, and the only things that have touched `develop` since are Renovate bumps, two review-tracking template re-syncs (#65, #66), one consistency-fix pull request (#71) and the prune bot. So assume a finding is live unless you can point at what fixed it: `already-fixed` **requires** naming the commit or the current code that resolves it, and a finding you cannot locate stays `still-present` for a human to look at. For each `still-present` finding give the current file and line. Check each against the open issue list before reporting it as new -- #13 (adversarial agent tests), #14 (clipboard password-manager hints), #15 (clipboard module split), #25 (waiting for a local X display) and #62 (D-Bus spike deferred work) already exist, and a finding that restates one of those should say so. Output one table: finding, source agent, severity, status, current location, existing issue if any. |
| 12g | low | management (planned: sonnet) | none | Record the derivation in the master plan so it never has to be done again, in its own commit, subject "Record the push audit's range derivation." The `Merged` column is already complete, so this step adds to the phase 12 section the 16 landings as the audit range, the empty-tree form phase 0 needs and why, and the seven-slice partition with its numbers. Put commits and ranges in that section, not in the `Status` column, which holds one vocabulary term. Do not restate the findings here; they go in step 12h's section. |
| 12h | medium | management | none | Management step, not a sub-agent step. Three parts. (i) Review 12f's table, decide fix-or-decline for every finding, and record the outcome in the master plan under a new *Items deferred from the push audit* heading, with a reason in writing for each declination. If the audit found nothing, that is one sentence and a real result. (ii) Fill in the master plan's *Bugs fixed during this work* section, which currently names two ryll bugs from phase 2 and nothing since -- twelve phases either fixed things or did not, and both are answers. (iii) Work through the nine success criteria one at a time and record a verdict with evidence for each, per decision 8. Then set the master plan's status and its `docs/plans/index.md` row: `Complete` only if every criterion is met and every finding is resolved or declined; otherwise `In progress` with the reason named in the plan. Note that the consistency criterion now reads "files no issues other than `review-coverage`", and that #85 is closed by this phase's own pull request. Fixes land as their own pull request; this step decides and records. |

### Execution notes

- **2026-11-18 (AEDT).** The back brief was approved. Two answers:
  all six judgment agents of 12e run together after the gate rather
  than security first, and this session has permission to push and
  open the pull request once the definition of done passes.
- Steps 12a and 12b ran in parallel. Only 12a writes the worktree;
  12b writes patches to a scratch directory and changes no tracked
  file, so they cannot collide.
- **The gate after 12b passed exactly.** The assembled patch is 365
  diff rows, 63,239 insertions, 2,560 deletions over 149 distinct
  paths, and the seven slices' rows, insertions and deletions each
  sum to those totals. Verified twice: by `--numstat` over the 16
  landings, and by `git apply --stat` on each of the seven written
  patch files. The root-commit failure the plan predicted reproduced
  verbatim (`fatal: ambiguous argument '60c0c4d^'`), and the
  empty-tree form against `e89539f` gives 17 files and 2,357
  insertions. Nothing downstream is auditing the wrong thing.
- 12a's sub-agent left a two-word line where its sentence met the
  existing prose; the management session rewrapped that paragraph
  before committing. The block copy itself was byte-exact, confirmed
  against an independent clone of development's `main`.
- **Correction: the master plan has nine success criteria, not
  eight.** Decision 8 and the definition of done said eight and
  enumerated six unannotated ones, having missed that the
  review-tracking criterion rewritten in phase 11's close-out is
  itself annotated. Three are annotated and six are not. Corrected
  above rather than left for step 12h to trip over.
- **The ryll audits this phase cites are not uniform, and two
  phases have nothing to cite.** Checked before 12h needs it: ryll
  #477 (phase 2), #487 (phase 3) and #534 (phase 7) each record a
  push audit in the pull request body, with findings taken and
  declined. Ryll #552, #554 and #557 (phase 8) and #578 (phase 9)
  record none -- not in the pull request body, and not in any commit
  message under the merge. So phase 8's and phase 9's ryll work was
  never push-audited in ryll. That predates this phase, belongs to
  another repository, and is not a finding of this audit; step 12h
  says so plainly rather than implying coverage, and it wants an
  issue in ryll.
- **Wave 1 (12c) passed.** `pre-commit run --all-files` in 5 s,
  `make lint` in 3 s and `make test` in 89 s, each exit 0, with no
  file modified. The devcontainer image and `target/` were already
  warm, so there was no cold build. The management session re-ran
  `make lint` independently (exit 0) and checked the test log
  directly: 16 `test result: ok` lines and no line reporting a
  non-zero failure count. `git diff --name-only develop..HEAD` is
  documentation plus `PUSH-AUDIT.md`, so any failure here would have
  been a regression on `develop` rather than a finding against this
  plan. Logs under `scratchpad/12c/`.
- **The unwrap dimension is settled mechanically rather than by
  grep.** 12d's raw scan found about 1,300 `.unwrap()`/`.expect()`
  hits and triaged them by guessing where `#[cfg(test)]` began,
  which is approximate because patch offsets are not tree lines. The
  stronger argument is that clippy already forbids it: the crate
  inherits `unwrap_used = "warn"`, `make lint` runs clippy with
  `-D warnings`, and wave 1 passed, so no production `unwrap()`
  exists. That was proved rather than assumed -- a
  `Some(1u8).unwrap()` inserted into `replaced_notify`, a production
  function, made clippy fail with ``used `unwrap()` on an `Option`
  value`` and `could not compile andris (lib)`; the file was then
  restored from a copy and `make lint` is green again. **The gap
  worth naming:** `expect_used` is not configured, so `.expect()` in
  production code is unlinted. Nothing in the audit range relies on
  that, but it is the one place this reasoning does not reach, and
  it is an advisory for 12h.
- **Two of 12d's readings were reversed, and neither is a finding.**
  It flagged `platform/x11/checks.rs` lines "125 and 154" as
  possibly production code on the grounds that they "sit above the
  worktree's test start (line 112)". The expects are at 146 and 154,
  and both are below 112, so both are inside `#[cfg(test)]`. It then
  flagged `server/main_channel.rs:250` the same way; in the tree
  every `unwrap()` in that file is at 342 or later, after
  `#[cfg(test)]` at 304, and "250" was a patch offset rather than a
  tree line. Checked directly rather than taken on report.
- **The range's coverage was checked, not assumed, and there is no
  hole.** Exactly 22 tracked files outside `.vscode/` are absent
  from `whole.patch`, and every one is accounted for by work that is
  not this plan's: the eight `andris/src/platform/qemu/*` files and
  `scripts/gen-qemu-qnums.py` came in pull request #56 on
  2026-10-29, the day after this plan's last landing, as the helper
  for kerbside's "Son of SPICE" spike; the five release scripts
  under `tools/` came in #34 on 2026-10-20, which its own
  description calls part of shakenfist/development's release-gating
  plan; `andris/src/test_logs.rs` came in #47 on 2026-10-27, the fix
  for issue #26, which is an issue fix outside the phase structure
  exactly as this plan's conventions require findings to be;
  `docs/own-session.md` came in #83 after the plan; `.skillsaw.yaml`
  arrived under a Renovate pull request; the four
  `.github/exported-config/*.json` files are generated by the
  config-export workflow; and the last is this phase's own plan
  file. So the audit sees the whole of the plan, and the `Merged`
  column is not missing a landing.
- **Consequence for 12e:** those files are present in the worktree
  but outside the audit range, so the whole-file reading that the
  `source-file-size` v2 rule asks for must not turn into findings
  against the qemu backend or `test_logs.rs`. Each 12e brief says
  so.

### Wave 2 findings (step 12e)

All six judgment agents reported. **No blocking finding.** Severities
below are the agents', as moderated by the management session's own
checks; each agent read the current worktree files as well as its
patch, so a finding is a statement about the tree unless marked
otherwise. Identifiers are for steps 12f and 12h to cite.

**Security, server half (2d-1, opus).** Six, one of which is the most
substantial finding of this audit.

| Id | Sev | Finding |
|----|-----|---------|
| S1 | medium | **Handshake-permit starvation.** `MAX_HANDSHAKES` is 8 and `MAX_HANDSHAKES_PER_PEER` is 4 (`server/mod.rs:77-87`), so two source addresses -- or two IPv6 /64s from one host, since `peer_key` buckets by /64 -- take every permit and hold them by simply not sending a link message, for 3 s each and far longer by advancing a stage or guessing a ticket. Every other connection is then refused. Unauthenticated and trivially sustained. The management session verified the caps and the doc comment, which states the guarantee "it still keeps one address from holding all `MAX_HANDSHAKES`": that sentence is literally true and operationally useless, because the attacker picks the number of addresses. 2c independently found those two numbers copied into `ARCHITECTURE.md:23`, `docs/running.md:346` and `:350` and `docs/reading-guide.md:194`, so the documentation repeats the reasoning. Fix: make the global pool a multiple of the per-peer cap, or admit against free permits, and release a permit held by a peer that has sent nothing |
| S2 | low | **Pre-authentication session-id oracle.** `link.rs:263-277` looks up `link.connection_id` and refuses with `BadConnectionId` before the ticket check, so an unauthenticated peer learns in one round trip whether a guessed 32-bit id is the live session; `:241-259` likewise discloses which channels are served and whether audio is on. Ids are CSPRNG over 2^32 and the ticket is still required. Fix: look up after the ticket check and return the same refusal as a wrong ticket |
| S3 | low | **Client-driven resize amplifies.** `agent.rs:523-545` into `display.rs:645-700`: each accepted `VD_AGENT_MONITORS_CONFIG` destroys and recreates the surface and reallocates the shadow, lossy and heat maps, then re-encodes the whole desktop. Tens of bytes in, megabytes out. **Same underlying defect as X4**, seen from the server side |
| S4 | low | **48 MiB transient on the lossy UTF-8 path.** `agent.rs:103-110` and `~700-712`: `from_utf8_lossy` on an all-invalid 16 MiB body allocates about 48 MiB before the length test, and the comment acknowledges the ordering. One main channel can hold about 32 MiB queued plus a 16 MiB reassembly buffer. Bounded but generous |
| S5 | low | **Log amplification in the key decoder.** `keymap.rs:189-196` keys its log-once set on the 16-bit E1 pair, so a client can emit about 65,536 `warn` lines per inputs channel, and relinking resets the decoder |
| S6 | info | Forged `ACK`s defeat flow-control pacing (`channel.rs:244-259` subtracts a window per `ACK` without accounting for what was sent; CPU only). Client `i32` motion deltas reach the platform unvalidated (`inputs.rs:437-447`). The ticket is never zeroized (`config.rs:209`). An unauthenticated peer can hold both RSA keygen permits (`link.rs:276-294`) |

2d-1 cleared five areas with evidence: untrusted parsing (every body
bounded by `O::MAX_CLIENT_BODY`, no unchecked indexing of wire data,
negative `DisplayInit` fields refused, u64 intermediates in rate
control); authentication apart from S2 (ticket via `open_private_file`
checking regular-file, owner-equals-euid and no group or other bits,
`Debug` redacted, compared with `subtle::ct_eq`, and **no path reaches
a message loop before authentication**); TLS and exposure (a
non-loopback plain listener refused unless TLS is configured, no
fallback to plain on a bad certificate); concurrency (no `unsafe`, no
hand-written `Send`/`Sync`, no lock across an await, ordered session
replacement); and path construction (no server path is built from
client data).

**Security, platform half (2d-2, opus).** Six. 2d-2 noted that S-side
authentication would raise X1 and X4 if any pre-auth path existed;
2d-1 found none, so they stand as written.

| Id | Sev | Finding |
|----|-----|---------|
| X1 | medium | **Unsolicited clipboard egress.** `x11/clipboard.rs:572-577`: at thread start, if any program owns `CLIPBOARD`, andris fetches and converts that text into its own memory -- with no client connected and before any ticket is presented -- and `:713-734` repeats it on every ownership change. Only `UTF8_STRING`/`STRING` are requested and no password-manager hint target is inspected. Verified by the management session. This **confirms and sharpens issue #14**: the defect is not only an unhonoured hint when a client asks, but that the fetch is eager and client-independent. Two real mitigations exist and are worth keeping: `changes()` calls `mark_unchanged()` so a newly connected client is not offered what was already there, and `offer(None)` withdraws with `CLIPBOARD_RELEASE`. Fix: inspect `TARGETS` for the hint atoms, and defer the fetch until a client has attached and announced `CLIPBOARD_BY_DEMAND` |
| X2 | low | **No clipboard opt-out.** Sharing is unconditional in both directions; `grep -n clipboard andris/src/config.rs` is empty, while `--no-audio` exists for the analogous case. Fix: `--no-clipboard` |
| X3 | low | **Crackable clipboard fingerprint in debug logs.** `platform/mod.rs:91-96`: `text_hash` is 32-bit FNV-1a over the clipboard text, logged at `debug` from `clipboard.rs:683` and several `agent.rs` sites. Its own doc comment says a short text can be recovered by trying candidates, so the hazard was known and the mitigation chosen was "debug only" -- which is thin, because debug logging is exactly what an operator enables to diagnose a clipboard problem. A decision rather than a defect |
| X4 | medium | **Unthrottled geometry change on the operator's real session.** `x11/randr.rs:355`, driven from `agent.rs:523-551`: nothing bounds how often a client may resize, so a client can thrash the real screen size, relaying out the operator's windows and stalling capture behind the RandR round trips. **Dimensions themselves are sound** -- `Size::check` enforces at least 1, at most 16384 a side and 64 Mpixel with a `checked_mul`, then `clamp` confines to `RRGetScreenSizeRange`. **Same defect as S3.** Fix: a minimum interval between accepted resizes, coalescing a burst to the last |
| X5 | low | **A failed resize can leave the desktop changed.** `x11/randr.rs:512-540`: `undo` restores in reverse but through `carry_on`, which logs and continues, and restores the physical size at 96 DPI rather than the original. A resize that fails mid-way with a failing undo leaves the desktop at an intermediate size and DPI, reported only in a log. Documented at `:507-511` |
| X6 | low | **The ticket is created in a directory already known to be open to others.** `install.rs:330-350`: `make_config_dir` returns `OpenToOthers { mode }` when the `0o077` bits are set and only reports it; `make_ticket` then creates the ticket there anyway. Verified. The file is `O_EXCL` 0600 so it cannot be read, but another user can unlink and substitute it. Fix: refuse to create a *new* ticket in such a directory |

2d-2 cleared four areas with evidence: input injection (keys go evdev
code to `EVDEV_NAMES` to XKB key name to *this server's* keycode, so a
client cannot name an arbitrary keycode; buttons come from a 7-variant
enum; the XTEST `detail` is never client-chosen; coordinates saturate
into i16; events go only to the configured display's root, and nothing
touches the X auth cookie; the queue is bounded at `INPUT_QUEUE_LIMIT`
8192 with a `RELEASE_RESERVE` of 512 so an owed release never blocks
behind a flood, and overflow fails `inject`, which closes the channel
-- fail-safe); both `unsafe` sites (libpulse signatures checked
against the headers with a layout test, **no `unsafe impl Send`/`Sync`
anywhere in the crate**, `PaSimple` is `!Send`/`!Sync` by
`PhantomData`, and in `x11/shm.rs` the memfd is sealed before mapping
so a read cannot `SIGBUS`, no Rust reference is ever made into the
mapping, `Mapping::copy` bounds-checks with `checked_add` before
`copy_nonoverlapping`, and the segment size is never client-supplied);
capture arithmetic (every rectangle edge widens to `u64`, `Frame::new`
checks stride and buffer length with checked arithmetic); and
`install.rs` path handling (no configuration value becomes a path
component, and `quote_executable` refuses control characters and
quotes, checked against systemd 257's manual with `systemd-analyze
verify`).

One informational note worth keeping: andris never consults the
screensaver or lock state, so XTEST input from a ticketed client wakes
a blanked screen and reaches a locker's grab. That is standard
remote-desktop behaviour and wants a line in the documentation rather
than a code change.

**Code quality, server half (2a-1, sonnet).** Four advisories, all
accretion across phases.

| Id | Finding |
|----|---------|
| Q1 | The test read-until-message loop is copy-pasted four times, each with a different chunk size: `main_channel.rs:362-373` and `:426-437`, `inputs.rs:~522-537`, `channel.rs:603`. A shared helper would stop a fifth copy |
| Q2 | **The `Stats` field list is written five times** -- `stats.rs:189`, `:341`, `:391`, `:502` and `:515` (verified) -- so the stream and audio blocks each had to touch all five places, and only the `since!` macro guards against a missing field. A forgotten `summary()` field compiles silently |
| Q3 | The recv-and-ignore select arm is duplicated: `cursor.rs:83-88`, `playback.rs:488-493`, and a drain loop of the same shape at `inputs.rs:126-129` |
| Q4 | `display.rs` repeats the `send(Mark)` then `frame_sent(..)` pair in `frame()` and `refresh()`, and spells `self.conn.session().counters()` about twelve times |

2a-1 confirmed that wire handling sits in `shakenfist-spice-protocol`
and codecs in `shakenfist-spice-compression`, with no hand-rolled
protocol framing in `server/`: the four `to_le_bytes`/`from_le_bytes`
hits are PCM in `playback.rs`, cursor pixels in `cursor.rs` and
scancodes in `keymap.rs`, none of them framing (verified). It found
nothing worth moving into ryll today.

**Code quality, platform half (2a-2, sonnet).** Seven advisories. The
first four are one theme and the most valuable code-quality result of
the audit.

| Id | Finding |
|----|---------|
| P1 | **The synthetic backend has drifted from the real one: `Gone`.** `synthetic/mod.rs:249-252`: `vanish()` sets the gone signal and fails damage subscriptions, but `capture`, `capture_all`, `size`, `inject`, `set_size` and `Clipboard::set` all still succeed afterwards, where the trait contract promises `Gone` (`platform/mod.rs:564`, `:666`) and X11 delivers it through `stopped()`. Verified. **So no test using the synthetic backend can exercise the server's gone-handling mid-capture or mid-resize** |
| P2 | Drift: the input queue. `InputSink` says `inject` errors when the queue is full, and X11 enforces `INPUT_QUEUE_LIMIT` plus `RELEASE_RESERVE`; synthetic never refuses and has no release reserve, so "queue full" and "a release survives a full queue" are untestable through it |
| P3 | Drift: the clipboard cap. X11 drops text over `MAX_CLIPBOARD_TEXT` with a warning; synthetic stores it unconditionally. Verified: the constant is enforced at `x11/clipboard.rs:373`, `:699`, `:830`, `:884` and `server/agent.rs:715`, `:783`, and never in the synthetic backend or `platform/mod.rs`. Milder than stated, because the server checks it too, so only platform-level tests are weakened |
| P4 | Drift: resize. Real RandR clamps to the server's range and refuses with "the nearest it allows"; synthetic has only accept, unchanged and refuse-all, so clamping and at-limit behaviour is untestable through it |
| P5 | `InputQueue` and its limits (`x11/input.rs:30-90`, `x11/mod.rs:129-137`) are a backend-neutral bounded FIFO and belong in `platform/`. The qemu backend has since copied the constants, which is out of range but confirms the abstraction |
| P6 | The three X event-loop threads (`x11/cursor.rs:62-110`, `x11/damage.rs:165-215`, `x11/clipboard.rs:572-632`) each repeat the same skeleton: `wait_for_event`, wake-cookie check, warn-once on X errors, `lost(..)` on failure |
| P7 | `SyntheticPlatform::platform()` installs a `ToneSource` that `run` then overwrites (`lib.rs:59-77`, `synthetic/mod.rs:234-244`), so the default is dead on the production path and the tone choice is decided twice |

2a-2 confirmed and refined **issue #15** rather than re-filing it: the
follow/serve seam is real, `follow.rs` would take `Fetch`, `Incoming`
and the INCR-in path (`:713-940`) and `serve.rs` would take `Owned`,
`Transfer` and `acquire` through `next_piece` (`:941-1155`), with the
atoms, timer, handle, `run` and `housekeeping` staying in the parent
and `earliest` staying because it spans both sides. It found no
`comment-proportion` problems: the long module docs carry contract and
rationale.

**Tests (2b, sonnet).** Six advisories. It verified the ryll pin
independently -- `38c51eff` with protocol and compression 0.3.0, in
both `andris/Cargo.toml` and `Cargo.lock`.

| Id | Finding |
|----|---------|
| T1 | **`VD_AGENT_CLIPBOARD_RELEASE` is sent and never parsed by a test.** `server/agent.rs:774` queues it; no occurrence anywhere in `andris/tests/`. Verified. It is wire output with no round trip, which is the one genuine gap in an otherwise complete table |
| T2 | The clipboard decode path without the SELECTION capability (`has_selection=false`) is untested: every test decodes with `true` |
| T3 | The inputs pointer messages (position, motion, press, release, wheel) have no truncated or short-body tests, though the key event does. These drive XTEST, so they are the priority of this group. Also untested: cursor-channel client messages, main `ATTACH_CHANNELS`/`PONG` short bodies, overflowing channel-cap counts, truncated display `INIT` |
| T4 | Fragile assertions on log text and formatting: `main_channel.rs:242-253` and `:285-319` match a log level, field names and an exact summary string; `stream.rs:667-671` and `:943-961` match a log line and the stats format; `stream.rs:825` and `:932-940` read session internals. Several timing-sensitive sleeps will flake on a loaded runner |
| T5 | **The X11 and PulseAudio tests skip silently and report green.** `tests/x11.rs` (six tests) and `tests/pulse.rs` (two) need a real X or sound server and are covered only by `make e2e`. A skip that reports success is the failure mode worth fixing: a skip counter, or a CI job asserting the environment variable is set |
| T6 | **The real X11 `InputSink` and `CursorSource` have no test at all**, though Xvfb provides XTEST and XFixes so they are testable rather than untestable. Server tests use the synthetic backend only, and the e2e cursor check asserts only a log line. This is the largest real-X coverage gap, and it compounds P1 to P4: the stand-in is weaker than the real thing *and* the real thing is untested |

2b confirmed that everything else andris puts on the wire is
round-tripped against the parser ryll's client uses, channel by
channel, and that **issue #13** is still accurate: none of its seven
adversarial cases appears in `tests/agent.rs`. It recommends widening
#13 with T1 and T2 rather than filing new issues.

**Documentation (2c, sonnet).** Three advisories, no blocking finding.

| Id | Finding |
|----|---------|
| D1 | The master-plan links in `AGENTS.md`, `ARCHITECTURE.md`, `README.md`, `docs/development.md` and `docs/index.md` present the plan as unfinished -- "how will it be built?", "the order in which it will be built", the authority for "what is still to be built". Allowed while the plan has unbuilt work; step 12h decides whether that is still true |
| D2 | `ARCHITECTURE.md` has grown the most of the three agent documents (194 lines) and several component-table cells are paragraph-length and carry tunables -- 8 permits, 4 slots, 64x64 tiles, 32 rectangles, 4 Mi -- that `docs/running.md` and `docs/reading-guide.md` also hold |
| D3 | Numbers duplicated between `ARCHITECTURE.md` and `docs/` currently agree with the code but will drift: the handshake limits, the 20-frames-in-2-seconds stream trigger, the default listen address. Point `ARCHITECTURE.md` at `docs/running.md` instead of restating them |

2c found **no plan-phase references in code or documentation** -- the
two apparent hits, `codeql-analysis.yml:18` and
`tools/ci-prune-reviews.sh:194`, cite development's "Adopting a
repository" procedure, not this plan -- and no contradictions between
documents. It checked all 14 documented `--` options against
`config.rs` and every documented `make` target and script path: all
exist, with matching defaults. It found nothing describing planned
behaviour as if it existed.

**Out of range, for issues rather than findings.** `ARCHITECTURE.md`
contradicts itself about the platform backends: line 35 documents the
qemu backend in full and line 36 calls its `shm.rs` "the crate's third
`unsafe` module", while the "Platform seams" commitment at line 186
still says "an X11 implementation and a synthetic one". This plan
wrote line 186; pull request #56 added the backend and left it stale.
Not this audit's finding, but it should be filed.

### Triage (step 12f), and why it ran in the management session

**Departure from the plan, stated so a reviewer can object.** Step 12f
was briefed as a sub-agent step on opus at high effort, to classify
every finding against current `develop` as `still-present`,
`already-fixed` or `moved`. That brief rested on a premise the run
disproved. The plan assumed the agents would read a patch three weeks
older than the tree, so some findings would already be fixed. In fact
each 12e brief told its agent to read the current worktree files as
well as its patch -- which the `source-file-size` v2 rule requires
anyway -- so every finding came back as a statement about the tree,
and `already-fixed` is not reachable by construction. The management
session also verified twelve of the findings against the tree while
reviewing the reports, and found three agent errors doing it (recorded
above). What remained was bookkeeping plus the open-issue cross-check,
which the management session is better placed to do than an agent that
would have to rediscover the context. So the triage is below rather
than in a seventh report, and the plan's 12f row is marked as having
run this way.

**Classification.** There are **32** findings, not the 29 first
recorded here: the earlier count was taken from the grouping below
rather than from the finding tables, and it dropped T4 and S6. All 32
are `still-present`. None is `already-fixed` or `moved`: nothing has
landed on `develop` since `76e444c` except this phase's own commits,
and the findings were read off that tree.

The per-finding table step 12f's brief asked for, with the current
location a follow-up fix will need:

| Id | Current location | Tracked in |
|----|------------------|------------|
| S1 | `server/mod.rs:77-87`, `admit()`, `accept_loop` `:502-505` | #87 |
| S2 | `server/link.rs:263-277`, with `:241-259` | #92 |
| S3 | `server/agent.rs:523-545` into `server/display.rs:645-700` | #88 |
| S4 | `server/agent.rs:103-110` and `~700-712` | #92 |
| S5 | `server/keymap.rs:189-196`, reached from `:148` | #92 |
| S6 | `server/channel.rs:244-259`; `server/inputs.rs:437-447`; `config.rs:209`; `server/link.rs:276-294` | #92, in its informational paragraph |
| X1 | `platform/x11/clipboard.rs:572-577` and `:713-734`; targets at `:199-205` | #14 |
| X2 | `config.rs`, by absence (no clipboard option exists) | #91 |
| X3 | `platform/mod.rs:91-96`, used at `x11/clipboard.rs:683` | #91 |
| X4 | `platform/x11/randr.rs:355` | #88 |
| X5 | `platform/x11/randr.rs:512-540` | #92 |
| X6 | `install.rs:330-350`, then `:355` | #92 |
| Q1 | `server/main_channel.rs:362-373` and `:426-437`; `server/inputs.rs:~522-537`; `server/channel.rs:603` | #93 |
| Q2 | `server/stats.rs:189`, `:341`, `:391`, `:502`, `:515` | #93 |
| Q3 | `server/cursor.rs:83-88`; `server/playback.rs:488-493`; `server/inputs.rs:126-129` | #93 |
| Q4 | `server/display.rs`, `frame()` and `refresh()` | #93 |
| P1 | `platform/synthetic/mod.rs:249-252`, against `platform/mod.rs:564`, `:666` | #89 |
| P2 | `platform/synthetic/mod.rs:504`, against `platform/x11/input.rs:30-90` | #89 |
| P3 | `platform/synthetic/mod.rs:569`, against `platform/x11/clipboard.rs:373` | #89 |
| P4 | `platform/synthetic/mod.rs:541-558`, against `platform/x11/randr.rs:87-93` | #89 |
| P5 | `platform/x11/input.rs:30-90`, `platform/x11/mod.rs:129-137` | #89 |
| P6 | `platform/x11/cursor.rs:62-110`; `damage.rs:165-215`; `clipboard.rs:572-632` | #93 |
| P7 | `lib.rs:59-77` with `platform/synthetic/mod.rs:234-244` | #93 |
| T1 | `server/agent.rs:774`; absent from `andris/tests/` | #13 |
| T2 | `andris/tests/agent.rs`, by absence | #13 |
| T3 | `andris/tests/inputs.rs` and `cursor.rs`, by absence | #13 |
| T4 | `andris/tests/main_channel.rs:242-253`, `:285-319`; `stream.rs:667-671`, `:825`, `:932-961` | #90 |
| T5 | `andris/tests/x11.rs`, `andris/tests/pulse.rs` | #90 |
| T6 | `platform/x11/input.rs`, `platform/x11/cursor.rs`, by absence | #90 |
| D1 | `AGENTS.md:39`; `docs/index.md:42`; `ARCHITECTURE.md:174` | #94 |
| D2 | `ARCHITECTURE.md`, the component table | #94 |
| D3 | `ARCHITECTURE.md:23`; `docs/running.md:346`, `:350`; `docs/reading-guide.md:194` | #94 |

T4 and S6 were the two the first count missed. S6 was in fact already
carried in #92's informational paragraph; T4 was tracked nowhere and
is added to #90, which is the test issue it belongs with.

**Against the open issues.** Five issues already exist that findings
touch, and the audit refines rather than duplicates them:

| Issue | Findings | Disposition |
|-------|----------|-------------|
| #13 adversarial agent-protocol tests | T1, T2, T3 | Widen. 2b confirmed none of #13's seven cases is covered, and adds the server-sent `CLIPBOARD_RELEASE`, the `has_selection=false` decode path and the inputs pointer messages' short bodies |
| #14 password-manager clipboard hints | X1 | Sharpen. The eager, client-independent, pre-ticket fetch is a stronger statement than the issue currently makes, and the fix list gains "defer the fetch until a client has attached" |
| #15 split the clipboard thread's module | 2a-2's refinement | Refine with the named follow/serve seam and the file list |
| #25 wait for a local X display | none | Untouched by this audit |
| #62 D-Bus display spike deferred work | none | Out of range, as the qemu backend is |

**New work the audit found.** Grouped as they would be fixed, not one
per finding, because several share a cause:

| Group | Findings | Why grouped |
|-------|----------|-------------|
| A. Handshake permits can be starved | S1 | On its own: it is the only finding with an unauthenticated remote denial of service, and its fix changes a documented invariant in four files |
| B. Client-driven resize is unbounded | S3, X4 | One defect, seen from both halves. One debounce fixes both |
| C. The synthetic backend has drifted | P1, P2, P3, P4, P5 | One cause -- the real backends grew and the stand-in did not -- and P5's shared `InputQueue` is part of the fix for P2 |
| D. Real-X paths are untested, skips are silent, and some tests assert on log text | T5, T6, T4 | The first two are both "CI reports green for code it never ran". T4 rides along because it is the same files and the same reviewer's pass |
| E. Clipboard exposure controls | X2, X3 | Both are operator-facing choices about clipboard sharing, decided together |
| F. Smaller server hardening | S2, S4, S5, S6, X5, X6 | Independent low-severity items, cheap together, none worth its own pull request. S6's four informational items ride in the issue's closing paragraph |
| G. Accretion tidy-ups | Q1, Q2, Q3, Q4, P6, P7 | Pure code quality, no behaviour change |
| H. Documentation consolidation | D1, D2, D3 | One pass over `ARCHITECTURE.md` and the duplicated numbers |

**The file-size dimension, which is why the runbook was brought
current.** Decision 2 fixed `source-file-size` to v2 before the audit
ran because v2 tells the auditor to read whole files rather than only
the diff, and to apply the 1,500-line ratchet by hand where a
repository has no ratchet, as andris does not. That dimension produced
a verdict for every file, and the verdicts belong here rather than
only in the issue they were summarised into:

| File | Lines | Seam | Verdict |
|------|-------|------|---------|
| `server/updates/stream.rs` | 1,681 | `RateControl`, `RateEvent`, `RateReason`, `RateChange` and the rate constants to `updates/rate.rs`, with their tests following | **Over 1,500, so it wants a stated reason to stay whole.** It is also the most-edited file in the plan. Split it when it is next touched |
| `platform/x11/clipboard.rs` | 1,362 | follow/serve, as the module doc's own headings: `follow.rs` and `serve.rs` | Split advisable. This is #15, confirmed and refined with the file list |
| `platform/synthetic/mod.rs` | 1,309 | pixel rendering (`Fill`, `Scene`, `render`, `photo_pixel`, `animated_pixel`) to `render.rs` | Optional. About 715 lines are non-test |
| `platform/damage.rs` | 1,296 | `Rectangle`/`Region` geometry against `Hub`/`DamageSubscription` | Justified. The two halves are independent, so a split is optional |
| `scripts/e2e-drive.py` | 1,241 | not assessed | **No verdict: see below** |
| `server/display.rs` | 1,189 | the stream lifecycle (`start_stream` through `disable_streams`, about 260 lines) to `display/streaming.rs` | The clearest split on the list |
| `andris/tests/stream.rs` | 1,110 | the `Viewer` harness to `tests/support/stream.rs`, as `support/display.rs` already does | Justified; split advisable |
| `config.rs` | 1,097 | weak: the private-file and ticket helpers | Justified. The length is mostly tests |
| `platform/x11/randr.rs` | 1,061 | the pure planning half (clamp, plan, verdict) against the X-calling half | Optional `randr/plan.rs`; one concept, so justified as is |
| `scripts/e2e-xrdp-drive.py` | 1,044 | not assessed | **No verdict: see below** |
| `server/agent.rs` | 1,023 | the flow-control types, and the clipboard handling to `agent/clipboard.rs` | Marginal, low priority. Length partly justified by protocol comments that should stay |
| `platform/mod.rs` | 989 | possible: `Frame`, `Patch`, `Capture` | Justified. It is the contract module |
| `andris/tests/x11.rs` | 950 | damage/capture/RandR against clipboard | Justified. Only six tests, so it is helper-heavy |
| `install.rs` | 949 | none natural | Justified |
| `server/channel.rs` | 920 | one cohesive transport | Justified |
| `platform/x11/mod.rs` | 919 | the shared thread plumbing to `x11/thread.rs`, leaving about 480 lines | Split advisable |
| `andris/tests/agent.rs` | 911 | `agent_clipboard.rs` if #13 adds ten more cases | Justified; leave |
| `andris/tests/display.rs` | 887 | shared parts are already in `support/display.rs` | Justified; leave |
| `server/stats.rs` | 845 | only `Reporter` and `summary`, not worth a file | Justified. See Q2 for the real problem in it |

Two notes on that table. The agents reported their line counts split
into non-test and test, and used the non-test figure to call
`channel.rs` and `stats.rs` justified. The shared block exempts
generated files, vendored trees and data tables -- not tests -- so the
`Lines` column above is the whole file, which is what the rule
measures; the non-test split is useful for deciding whether a split is
worth doing, not for deciding whether the rule applies.

**And a gap this phase should own rather than bury: the two Python
end-to-end drivers got no size verdict.** `scripts/e2e-drive.py`
(1,241 lines) and `scripts/e2e-xrdp-drive.py` (1,044) are both over
the advisory line, and both fell in the `harness` slice, which
decision 3 gave only to the documentation agent -- for checking
documented build and CI facts -- and to no code-quality agent.

**The size verdicts are the smallest part of that gap.** The
`harness` slice is 12,039 insertions of GitHub workflows, shell and
Python scripts, the packaging and the devcontainer, and it received
**no code-quality and no security review at all**. What went
unexamined is not just two long Python files: it is untrusted-input
handling in workflow expressions, token scope and permissions blocks,
what runs on a self-hosted runner, the kerbside lane's compose stack
and the wheel build. Those are a different hazard class from the
server's wire parsing, and a SPICE server's CI is a supply-chain
surface in its own right. The one concrete consequence already found
is the plan link in the systemd unit, which the clean-areas claim
above wrongly denied existed.

That is a hole in decision 3 rather than in any agent's work. It is
recorded in #93, and the right fix for a future audit of this
repository is to give the harness slice its own code-quality and
security reviewer rather than treating it as documentation context.

**On publishing an unfixed denial of service in the open.** This
record, issue #87 and this phase's pull request all state the
constants, the mechanism and the minimum resources for S1 while it is
unfixed on `develop`, and the same for S2's pre-authentication oracle.
The review round raised that, and the operator's decision on
2026-11-18 was to leave it and say why.

The reasoning, so a later reader can disagree with it knowingly.
Andris is pre-1.0 with one deployment, the operator's own. Its default
listener is `127.0.0.1:5900`, and a non-loopback plain listener is
refused unless all three TLS options are given (`config.rs:273-279`),
so reaching the handshake from off the host means going through
kerbside, which authenticates separately. Against that, an audit whose
findings cannot be checked is not an audit: a reader who cannot see
the mechanism cannot tell whether S1 is real, and this record exists
to be checked rather than believed. The detail is in any case already
published across this branch's commits, #87 and the pull request body,
so redacting it now would cost the record its checkability without
un-publishing anything. The window closes by fixing #87, which is
where the effort belongs.

**Out of range, so an issue rather than a finding.**
`ARCHITECTURE.md`'s "Platform seams" commitment names two backends
where its own inventory documents three.

**Informational, needing documentation rather than code.** Andris
never consults the screensaver or lock state, so a ticketed client's
XTEST input wakes a blanked screen and reaches a locker's grab. That
is what every remote-desktop server does, and it should be stated in
`docs/` rather than changed.

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
  nine success criteria carry no evidence either way.
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
  The extracted block and development's template both hash to
  `ec6cdfae29fd58fa6fe672ae9309550070dc4522`, recorded here so the
  claim can be checked without a clone: `git hash-object` on the
  block's byte range, including its trailing newline, prints it. This
  check prints `ok`, needs network access, and removes its clone
  afterwards:

  ```
  python3 -I - <<'EOF'
  import pathlib, re, shutil, subprocess, tempfile
  d = tempfile.mkdtemp()
  try:
      subprocess.run(['git', 'clone', '--depth', '1', '-q',
                      'https://github.com/shakenfist/development', d],
                     check=True)
      want = pathlib.Path(
          d, 'templates/shared-blocks/source-file-size.md').read_text()
      have = open('PUSH-AUDIT.md').read()
      m = re.search(
          r'<!-- shared-block: source-file-size v\d+ -->'
          r'.*?<!-- shared-block-end -->\n', have, re.S)
      assert m, 'block not found'
      assert m.group(0) == want, 'block differs from development main'
      print('ok')
  finally:
      shutil.rmtree(d, ignore_errors=True)
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
- Each of the master plan's nine success criteria carries a verdict
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
nine criteria with verdicts and let them decide.

## Result

**Complete, 2026-11-18.** Wave 1 passed and there was no blocking
finding. **32 findings**, all still present on `develop`, all tracked:
eight new issues (#87 to #94) grouped by cause, and three existing
issues sharpened (#13, #14, #15). Nothing was declined. The first
version of this Result said 29, counted from the grouping rather than
the finding tables, and so dropped T4 and S6; the per-finding table in
the triage section is now the authority and reconciles to 32.

"All still present" was true on the day. Twenty-eight of the
thirty-two were fixed after this phase merged: #87, #88, #91 and #92,
the security ones, in the follow-up that filled its `Merged` cell, and
#89, #90, #93 and #94, the quality ones, in #110. The
master plan's deferred-items section tracks what is left and is the
place to read for the current state; this Result is left as the
record of what the audit found.

The assembled patch matched the plan's four gate numbers exactly --
365 diff rows, 63,239 insertions, 2,560 deletions, 149 distinct paths
-- verified twice, once by `--numstat` over the 16 landings and again
by `git apply --stat` on each written patch file. The seven slices
partition it with rows, insertions and deletions each summing to those
totals: server 46/13,012/720, platform 47/12,120/318, core
15/2,382/95, tests 30/8,361/529, harness 119/12,039/371, docs
61/4,889/310, plans 47/10,436/217. The derivation is recorded in the
master plan so it never
has to be done again, including the empty-tree form phase 0 needs
because its range starts at the repository's root commit.

The audit saw the whole of the plan: exactly 22 tracked files outside
`.vscode/` are absent from the patch and each belongs to work that is
not this plan's, so the `Merged` column is not missing a landing.

The three findings worth a reader's attention, in order:

1. **#87**, the only unauthenticated denial of service. `MAX_HANDSHAKES`
   8 against `MAX_HANDSHAKES_PER_PEER` 4 means two addresses take
   every permit and hold them by saying nothing. The code's own
   comment asserts a guarantee here that is literally true and
   operationally useless, and three documents repeat the reasoning.
2. **#89 with #90**, the pair that matters most for confidence in the
   test suite. The synthetic backend no longer models the real one on
   four counts, *and* the real X11 input and cursor paths have no test
   at all, *and* the tests that would need a real X server skip
   silently and report green. Each is survivable; together they mean
   the path from a client's bytes to events on the operator's desktop
   is covered by nothing that runs in CI.
3. **#14 as sharpened.** Andris copies the operator's clipboard into
   its own memory at thread start and on every ownership change, with
   no client connected and before any ticket is presented.

**What this audit did not cover, stated as a limit on the phase
rather than a footnote.** A seventh of the audited diff got no
code-quality and no security review, and the phase is `Complete`
anyway. That is defensible only because it is said plainly: every
step of the plan ran, and the gap is in what the plan asked for, not
in what was delivered against it. A reader should treat this audit as
covering `andris/src` and the documentation thoroughly, and the
build, CI, scripts and packaging only for whether the documented
facts about them are true. #93 carries the remainder.

One thing the split got wrong, recorded in the triage section: the
`harness` slice -- 12,039 insertions of scripts, CI and packaging --
went to the documentation agent and to no code-quality agent, so two
Python end-to-end drivers over 1,000 lines each got no size verdict.
That is a hole in decision 3, not in an agent's work.

The clean areas are recorded in full above, because an audit that
lists only problems cannot be checked. The ones worth repeating: no
path reaches a message loop before authentication; there is no
`unsafe impl Send` or `Sync` anywhere in the crate; the memfd is
sealed before mapping so a read cannot `SIGBUS`; a client cannot name
an arbitrary X keycode; and every wire body is bounded before
buffering.

Two clean-area claims in the first version of this Result were
overstated, and the review round caught both.

"No plan-phase reference appears in any code or documentation file"
was **false**. `packaging/systemd/andris@.service:52` linked the
master plan for `SystemCallFilter`, with the note "On trial, pending
a week of daily use with MIT-SHM capture and sound, which decides
whether it stays" -- and that trial had completed: phase 10's soak ran
2026-10-19 to 10-27 with MIT-SHM capture and sound and raised no
`status=31/SYS`. So by the `plan-references-in-code` block's own rule
("the link comes out when the work lands") it should have come out at
phase 10. It is removed here, with the trial's outcome recorded in its
place, because leaving it while claiming it did not exist is worse
than either. **This miss traces straight to the harness-slice gap
below:** the unit file is in the `harness` slice, which no
code-quality agent read, and 2c -- which had the slice for build and
CI facts -- grepped the code slices rather than its own second patch.

"No production `unwrap()` or `expect()` exists" was **true but not
shown by the evidence given**. The clippy argument covers
`unwrap_used` only; `expect_used` is not configured, as the execution
notes say, so `.expect()` needed enumerating rather than linting. The
enumeration: eight sites, all test code -- `config.rs:755`
(`cfg(test)` at 493), `platform/audio.rs:432` and `:438` (317),
`platform/x11/checks.rs:146` and `:154` (112),
`server/inputs.rs:562` (494), `server/updates/stream.rs:1190` (845),
and `server/updates/test_support.rs:350`, whose file carries no inner
marker because `updates/mod.rs:69-70` declares the whole module
`#[cfg(test)]`. 2d-1 reported "the only two", having looked only at
`server/`.

Two steps did not run as briefed, both recorded in the execution
notes with the reason: 12f, because its premise that findings would
need checking against a newer `develop` was disproved by the agents
reading current files, and 12g, because the skill for push-audit
phases puts the record in the management session. The master plan's
phase 12 row reads `Complete`; the plan itself stays `In progress`,
with its two unmeasured performance criteria named as the reason.
