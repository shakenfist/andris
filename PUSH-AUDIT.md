Thanks for your work on this. I appreciate it. Some final
checks before I push.

## How to use this runbook

The pre-push audit splits into two waves:

**Wave 1 -- mechanical.** Build verification, lint and the test
suite. Always run wave 1 first; wave 2 is only worth spending on if
wave 1 passes.

**Wave 2 -- judgment.** Code-quality, test-coverage, documentation
and security review. Some of this is mechanical (a TODO, unwrap and
`unsafe` scan of the diff); the rest needs sub-agents to read code
and apply judgment. The four judgment agents are independent and
can be spawned in parallel.

The management session reviews all findings, fixes any issues, and
confirms the push.

## Two ways this runbook is invoked

**As a pre-push gate.** The classic use: an unpushed branch, about
to become a pull request. The range audited is `develop...HEAD`,
which is what the briefs below use by default, and findings are
fixed on the branch before the push.

**As a master plan's closing phase.** Every master plan ends with a
push-audit phase. That plan's phases each landed separately, so by
the time the audit runs, `develop...HEAD` on the audit branch holds
nothing but the audit's own edits. Auditing that reports "no
findings" for the wrong reason, which is the failure the phase
exists to prevent.

What is wanted is the plan's accumulated diff, and it has to be
*recorded*: unrelated work lands on `develop` between phases, so
"everything since the plan file appeared" is far too wide, and the
phase branches are gone by then. As each phase of a master plan
lands, what landed it goes in the `Merged` column of the plan's
phase table. It does not go in the `Status` column, which the
`plan-status-vocabulary` shared block reserves for a single term.

A phase that landed as a pull request records its merge commit,
whose diff against its first parent is the whole of it. A phase
that landed directly on `develop` records every commit of the
phase, or its `first..last` range; a single commit is only ever
enough when it is a merge commit. A phase that landed in ryll
records `ryll <sha> (#pr)` and was audited there, as part of that
pull request; cite that audit rather than repeating it.

Given those commits, build the combined patch and hand the judgment
agents its path, rather than a revision range:

```
for m in <the phase merge commits>; do
    git diff "$m^1" "$m"
done > /tmp/plan-audit.patch
```

For a phase recorded as a `first..last` range, append
`git diff first^ last` instead. In the briefs below,
`git diff develop...HEAD` means "the audit range", which in this
mode is that patch. Wave 1 runs against the tip of `develop`, which
holds every phase.

If the phase table records no commits, reconstruct what you can
from `gh pr list --state merged`, `git rev-list --first-parent` and
the phase plan filenames, say in the findings how much of the plan
you were actually able to see, and write the commits into the table
so the next audit does not start from nothing. Do not reconstruct
from a path-filtered `git log` alone: it lists the commits that
touched a path without saying which arrived directly and which
arrived inside a pull request.

Three items in the management checklist at the end read
differently here. Findings become their own pull request against
`develop` rather than fixes on an unpushed branch; the
commit-history and up-to-date items apply to that follow-up pull
request; and "ready to push" becomes "every finding is fixed, or
declined in writing in the master plan".

## Wave 1: Mechanical checks

Run, from the repository root:

```
pre-commit run --all-files
make lint
make test
```

`make lint` runs rustfmt and clippy with `-D warnings`, and
`make test` runs the test suite; both run cargo inside Docker.
Never run cargo on the host. Until the repository has a Cargo
workspace and a `Makefile`, wave 1 is `pre-commit run --all-files`
alone, and the audit report says so.

Every command must pass. If one fails, fix the cause and re-run
before spending on wave 2.

### Style conformance -- judgment portion

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | low |

**Brief for sub-agent (only if wave 1 passes):**

Check `git diff develop...HEAD` for adherence to the conventions
and invariants in `AGENTS.md`:

- Every SPICE message type andris sends is named in ryll's
  `logging::message_names` tables (in `shakenfist-spice-protocol`).
  A message type ryll does not model is a blocking finding: kerbside
  terminates the session on it.
- Nothing depends on `shakenfist-spice-renderer`. Wire types come
  from `shakenfist-spice-protocol`, and codecs from
  `shakenfist-spice-compression`.
- Platform concerns (capture, input injection, display control,
  clipboard, audio) sit behind their traits rather than being
  called directly from protocol code.
- Field rename and unit-change discipline: did any field silently
  change units (seconds to milliseconds, say) without a rename or
  a doc comment?

Report a short list of any violations found. If none, say "Style
checks passed."

## Wave 2: Deeper review

Only run wave 2 after wave 1 passes. Spawn the judgment agents
below; they can run in parallel.

### 2a. Code quality

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | medium |

**Brief for sub-agent:**

Review the diff (`git diff develop...HEAD`). Start with a
mechanical scan of the added lines for TODO, FIXME, HACK and XXX,
new `#[allow(dead_code)]`, new `unsafe` blocks, and new `.unwrap()`
and `.expect()` calls. Then add the judgment-level review:

- **Duplicated code:** are there significant blocks of duplicated
  logic? Look for copy-paste across channel handlers, and for wire
  handling that belongs in ryll's shared crates rather than here.
- **Missed abstractions:** should any new code be extracted into a
  shared module, or into ryll, where kerbside could use it too?
- **Triage the scan's raw findings:** for each TODO, unwrap or
  `unsafe` found, say blocking or advisory and why. Skip ones in
  `#[cfg(test)]` blocks.

<!-- shared-block: comment-proportion v1 -->
Comment proportion (shared block; do not edit -- the canonical
copy lives in shakenfist/development at
`templates/shared-blocks/comment-proportion.md`):

- A comment or docstring earns its length by saying what the code
  cannot: the contract, the units, the failure modes, the reason a
  surprising choice is correct. Restating the code in prose is not
  documentation.
- Treat as candidates any added comment or docstring that is longer
  than the code it documents, and any comment block over roughly
  fifteen lines attached to a body under ten. These are candidates,
  not verdicts -- a subtle algorithm, a public API contract, or a
  hard-won bug explanation can justify the length.
- Where the length is not justified the finding is advisory, and
  the fix is to cut the restatement rather than delete the comment:
  keep the why, drop the line-by-line narration of the what.
- Prose that documents user-visible behaviour rather than the
  implementation usually belongs in `docs/`, with the comment
  reduced to a pointer.
<!-- shared-block-end -->

<!-- shared-block: source-file-size v1 -->
Source file size (shared block; do not edit -- the canonical
copy lives in shakenfist/development at
`templates/shared-blocks/source-file-size.md`):

- Where a repository tracks whole-file human review, a file's cost
  is its length times how often it is touched: every change
  discards the review of the whole file, and the next session
  re-reads all of it. That, rather than taste, is why length is
  worth raising in review at all.
- Treat a source file over roughly 800 lines as a candidate to
  split, and one over roughly 1,500 as wanting a stated reason to
  stay whole. These hold whether or not a repository tracks review
  per file: tracking is what makes the cost repeat and become
  measurable, not what makes a long file expensive to read. Both
  are advisory. Neither is a gate, there is no hard cap, and a
  reviewer who raises one is opening a question, not recording a
  defect.
- Generated files, vendored trees and protocol or data tables are
  exempt: they are not read the way source is, and a tool that
  counts them is measuring the wrong thing.
- Split along a seam that already exists -- one module's public
  entry point, one check, one subcommand, one endpoint -- so that
  a later change touches one of the pieces rather than all of
  them. A file split at a line number rather than at a seam is
  worse than the long file it replaced.
- Length is never reduced by deleting the comments and docstrings
  that explain why the code is the way it is. Those are what make
  a long file reviewable, and trading them for a line count makes
  the review worse while making the number better. Cut duplicated
  scaffolding first; see `comment-proportion` for what earns its
  length.
<!-- shared-block-end -->

<!-- shared-block: python-version-discipline v1 -->
Python version and typing (shared block; do not edit -- the
canonical copy lives in shakenfist/development at
`templates/shared-blocks/python-version-discipline.md`):

- No syntax or standard library API newer than the floor in
  `requires-python`. Structural pattern matching, `X | Y` unions in
  annotations evaluated at runtime, `tomllib`, and
  `datetime.UTC` each raise on an interpreter the package still
  claims to support, and none of them fail in CI when CI runs only
  the newest version. This is the finding to look for first: it is
  a real break on a real user's machine, not a style point.
- New and modified code carries type hints, and mypy is expected to
  be clean over it. A project part way through a staged rollout is
  held to the new code, not to the whole tree.
- Prefer the walrus operator and f-strings where they make the code
  read better, subject to the floor above.
- Raising the floor in `requires-python` is a supported-platforms
  decision, not a convenience: it drops users. If it is genuinely
  right, the platforms table, `requires-python` and
  `constraints.python` in `renovate.json` all move together.
<!-- shared-block-end -->

Report findings as a bullet list. For each finding, state the file,
line, and whether it's blocking (must fix before push) or advisory
(can address later).

### 2b. Test review

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | medium |

**Brief for sub-agent:**

Review the diff (`git diff develop...HEAD`) for test coverage:

- Does every new public function or significant code path have
  test coverage?
- Is everything andris writes to the wire round-trip tested against
  the parser ryll's client uses?
- Do the tests include adversarial cases (malformed client
  messages, empty data, overflow values)?
- Are there any assertions that test implementation details rather
  than behaviour (fragile tests)?

Also verify that all existing tests still pass (wave 1 already
confirmed this, so just check the wave 1 result).

<!-- shared-block: functional-test-coverage v1 -->
Functional test coverage (shared block; do not edit -- the
canonical copy lives in shakenfist/development at
`templates/shared-blocks/functional-test-coverage.md`):

- The standard is "do we run the code to do the real thing, and
  does it work as intended". Every subcommand exposed on the command
  line, and every endpoint exposed by an API, should have a test
  that exercises it for real rather than against a mock of itself.
- For a change that adds or alters user-visible behaviour, the
  question to answer is which functional test would have failed
  before it and passes after. If there is none, that is the finding,
  and it is a finding about this change rather than a note for
  later.
- Unit tests are held to no coverage percentage, but a branch that
  is reachable from outside the process and has no test is worth
  naming. Error paths and argument validation are where this bites:
  they are the code most often written once and never run again.
- Mocking the system under test proves nothing. Mock the boundary --
  the network, the clock, the hypervisor -- and let the code being
  tested actually run.
- Where a gap is real but out of scope for the change in hand, say
  so plainly and record it, rather than silently widening the
  change or silently leaving it unsaid.
<!-- shared-block-end -->

Report findings as a bullet list grouped by file.

### 2c. Documentation review

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | medium |

**Brief for sub-agent:**

Check that documentation matches the current code state. Read the
diff (`git diff develop...HEAD`) and verify:

<!-- shared-block: readme-discipline v1 -->
README discipline (shared block; do not edit -- the canonical
copy lives in shakenfist/development at
`templates/shared-blocks/readme-discipline.md`):

- New user-visible features are documented in `docs/` (and
  `ARCHITECTURE.md` / `AGENTS.md` where appropriate), not by
  adding bullets to `README.md`.
- `README.md` is a pitch: what the project is, who it is for,
  minimal installation instructions, a small number of usage
  examples, and curated absolute links into `docs/`. It only
  changes when the pitch, the install story, or the
  documentation links change.
- README growth is itself a finding: if the diff adds README
  content that belongs in `docs/`, flag it as blocking and
  move it.
<!-- shared-block-end -->

<!-- shared-block: llm-doc-discipline v1 -->
AGENTS.md and ARCHITECTURE.md discipline (shared block; do not
edit -- the canonical copy lives in shakenfist/development at
`templates/shared-blocks/llm-doc-discipline.md`):

- `AGENTS.md` is a working guide: the conventions, invariants and
  gotchas an agent cannot infer by reading the code, plus curated
  links into `docs/`. It is loaded into every session, so every
  line costs context on every task.
- `ARCHITECTURE.md` is a map: the component inventory, how data
  moves between components, and why the shape is the way it is.
  A deep dive on one subsystem belongs in `docs/`, where humans
  benefit from it too.
- One canonical home per fact. If `docs/` covers it, link to it
  instead of restating it -- and the same rule applies between
  `AGENTS.md` and `ARCHITECTURE.md`.
- Neither file is a reference manual, a runbook, or a changelog.
  CLI flags, configuration keys, wire protocols, step-by-step
  procedures and plan history go to `docs/`.
- Growth in either file is itself a finding: if the diff adds
  content that belongs in `docs/`, flag it as blocking and move
  it.
<!-- shared-block-end -->

<!-- shared-block: diagram-discipline v1 -->
Diagram discipline (shared block; do not edit -- the canonical
copy lives in shakenfist/development at
`templates/shared-blocks/diagram-discipline.md`):

- A diagram of *structure or flow* -- components and the arrows
  between them, an ordered exchange of messages, a state machine
  -- is written as a fenced `mermaid` block, not drawn in ASCII.
  GitHub renders those natively and the mkdocs sites render them
  through `pymdownx.superfences`, so the same source is a picture
  in both places.
- Not every box of characters is a diagram. These stay as plain
  code fences, because mermaid cannot express them and would lose
  what they show: directory and file trees; memory maps, address
  space layouts and register or bit-field diagrams, where column
  alignment carries the meaning; wire-format and on-disk byte
  layouts; captured terminal output; and tables. The test is
  whether the picture is nodes and edges. Something that is a
  table with lines drawn on it is a table.
- Pick the diagram type that matches the claim: `flowchart` for
  components and data flow, `sequenceDiagram` for an ordered
  exchange between parties, `stateDiagram-v2` for a state
  machine, `erDiagram` for data relationships. A sequence drawn
  as a flowchart has thrown away the ordering it existed to show.
- A new ASCII box-and-arrow diagram in the diff is a finding.
  Converting one the diff already touches is in scope; converting
  every other diagram in the file is not, because a sweep is its
  own change and its own review.
<!-- shared-block-end -->

<!-- shared-block: plan-phase-references v1 -->
Plan phase references (shared block; do not edit -- the canonical
copy lives in shakenfist/development at
`templates/shared-blocks/plan-phase-references.md`):

- Documentation outside plans directories describes the current
  state of the software, not the history of how it was built. Do
  not write "implemented in phase 5" or "since phase 3 of the
  two-tier CI plan": a reader wants to know whether a feature
  exists, not which phase of which plan delivered it.
- If a documented behaviour is implemented, describe it plainly.
  If it is planned but not yet implemented, link to the master
  plan in `docs/plans/` instead of citing a phase number.
- Reserve the word "phase" for plan documents. A procedural
  document describing a live multi-stage process (a release
  runbook, say) should call its stages "steps" or "stages", so
  that a phase reference in `docs/` is always a plan smell.
- The consistency audit greps `README.md` and `docs/` (excluding
  plans directories) for "phase <number>". Append
  `<!-- audit-ok: phase-reference -->` to a line only when the
  reference is genuinely not about an implementation plan.
<!-- shared-block-end -->

<!-- shared-block: plan-references-in-code v1 -->
Plan references in code (shared block; do not edit -- the
canonical copy lives in shakenfist/development at
`templates/shared-blocks/plan-references-in-code.md`):

- Code, comments, docstrings, test names, fixture descriptions and
  configuration describe the software as it is now. Which plan,
  phase, step or decision produced a line is history, and the plan
  and the commit log already keep it. Do not write "added in phase
  5", "per decision 3", "pending step 5f" or "the phase-4 leaks
  pass": a reader of the code has not read the plan, and the
  number tells them nothing.
- Where a comment cites a plan to explain why the code is the way
  it is, the explanation belongs in the comment. Write the reason
  -- the constraint, the measurement, the failure it prevents --
  and drop the citation. A pointer standing in for the reasoning
  costs every reader a detour, and rots when the plan is archived
  or renumbered.
- A plan link is acceptable only for work that is not built yet: a
  deliberate gap or refusal whose lifting is planned, where
  "deferred; see `PLAN-foo.md`" tells the reader the gap is known.
  The link comes out when the work lands. Prefer an issue link
  where one exists, and write a plan in another repository as an
  absolute URL; the `plan-source-references` audit checks that
  these links resolve.
- Plan documents and commit messages may cite phases and decisions
  freely; recording that history is their job.
- "Phase" in its ordinary sense -- a two-phase commit, a compiler's
  link phase -- is not a plan reference.
- A plan reference a diff adds to code is a finding to fix before
  pushing. References on lines the diff does not touch are backlog,
  not findings against the change.
<!-- shared-block-end -->

- No document outside `docs/plans/` describes planned behaviour as
  if it exists. Intent is a link to the master plan.
- `ARCHITECTURE.md` reflects any new or changed component, channel,
  platform trait or relationship to ryll and kerbside.
- `docs/` reflects any new configuration, dependency or build
  command, and `AGENTS.md` any new convention an agent could not
  infer by reading the code.
- Plan files in `docs/plans/` are up to date -- completed phases
  are marked complete, deferred items are listed.
- If the way andris speaks SPICE changed, note whether ryll's or
  kerbside's documentation needs review.

Report findings as a bullet list. "No documentation gaps found" is a
valid answer.

### 2d. Security review

| Setting | Value |
|---------|-------|
| Model | opus |
| Effort | high |

**Brief for sub-agent:**

Security review of the diff (`git diff develop...HEAD`). This
requires careful judgment -- read the actual code, not just the
diff summary. Andris is a network server: every byte a client sends
is untrusted.

Check for:

- **Input validation:** could malformed client messages cause
  panics, buffer overflows, or excessive memory allocation? Look for
  unchecked indexing, unbounded allocations based on
  client-controlled lengths, and arithmetic overflow.
- **Authentication:** is the ticket handled safely -- read from a
  file only its owner can read, never logged, compared without
  leaking timing? Is there any path that reaches a channel before
  authentication succeeds?
- **TLS and exposure:** is the listening address what the operator
  configured? Could a non-loopback listener run without TLS? Is
  certificate and key handling sound?
- **Desktop access:** andris acts on the session user's X display.
  Could a client inject input or read pixels beyond what a SPICE
  session should allow, or reach the X server's credentials?
- **Unsafe code:** are there any new `unsafe` blocks? If so, is the
  safety invariant documented and sound?
- **Concurrency:** are there new shared-state patterns (Arc, Mutex,
  atomics)? Could they deadlock or race?
- **Resource exhaustion:** could a malicious or slow client cause
  unbounded memory growth, file descriptor leaks, or CPU spin?

<!-- shared-block: path-traversal-review v1 -->
Path construction from outside data (shared block; do not edit --
the canonical copy lives in shakenfist/development at
`templates/shared-blocks/path-traversal-review.md`):

- Treat as a candidate any filesystem path built from a value the
  process did not choose: a request parameter, an image name, tag or
  digest, a layer path, an archive member name, a filename out of a
  configuration file or a database row.
- The question is not whether the value looks dangerous but whether
  the resulting path is *proved* to stay inside its intended base
  directory. Resolve the joined path with `os.path.realpath()` and
  verify it still starts with the base; a check on the untrusted
  component alone is defeated by symlinks and by encodings the
  check did not anticipate.
- Prefer a helper that cannot be forgotten at a call site --
  `safe_path_join()` in occystrap, or the framework's own
  (`send_from_directory` in Flask) -- over an inline guard repeated
  at each join.
- Archive extraction is the case most often missed: a member name
  inside a tarball or zip is attacker-controlled in exactly the same
  way as a request parameter.
- Where a bare join is correct because every component is
  process-chosen, say so in a comment rather than leaving the
  reader to re-derive it.
<!-- shared-block-end -->

Report findings with severity (critical / high / medium / low /
informational). For each finding, state the file, line, the
vulnerability class, and a recommended fix.

## Management session checklist

After all agents complete, the management session should:

- [ ] Wave 1 passed (build, lint, tests).
- [ ] Wave 2 findings reviewed.
- [ ] Any blocking findings from 2a/2b/2c have been fixed and
      re-verified.
- [ ] Any security findings from 2d have been assessed -- critical
      and high must be fixed before push.
- [ ] The commit history is clean (no fixup commits that should be
      squashed, no accidental files).
- [ ] The branch is up to date with the target branch (rebase if
      needed).
- [ ] Ready to push. (As a master plan's closing phase: every
      finding fixed, or declined in writing in the master plan.)
