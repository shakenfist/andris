# Phase 11: Human review

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: medium, as the master plan asks. Adopting review
tracking follows a procedure that shakenfist/development documents and
that seven repositories have already followed. Two parts call for
judgement. The first is the reading guide, which has to be right about
code nobody has explained yet. The second is the order in which
`next` offers files (decision 3).

## Scope

In scope:

- **Adopting andris into review tracking**, by the numbered steps of
  [Adopting a repository](https://github.com/shakenfist/development/blob/main/docs/code-review-tracking.md#adopting-a-repository):
  - the `.gitignore` exceptions;
  - the pre-commit excludes;
  - `.vscode/review-scope.toml`;
  - the three template files, byte for byte;
  - a generated `REVIEWS.md`;
  - CI that skips review-only changes;
  - the prune bot's push token.
- **A reading guide**, `docs/reading-guide.md`. It gives an order to
  read andris in, from the binary's entry point through the server,
  the channels and the platform backends. For each file it says what
  the file does and what to look at.
- **Adding andris to development's table of adopters**, as one small
  pull request in shakenfist/development.
- **The operator's review sessions**, until every in-scope file
  carries a current review mark. Each session lands as its own pull
  request. Findings become issues, and their fixes are their own pull
  requests.

Out of scope:

- **Fixing what the review finds, inside this phase's pull requests.**
  A finding is filed as an issue and fixed in a pull request of its
  own, so that review-state commits stay review state.
- **Changing development's review tooling.** If andris needs a
  feature the tooling lacks, the plan records it, and the fix belongs
  in shakenfist/development.
- **Ryll's shared crates.** Andris depends on ryll's protocol and
  compression crates by git revision. Their review is ryll's, and
  ryll already has it.

## What the survey found

The master plan's phase 11 section is right in outline, but several
claims in it need correcting. The corrections are made at the source,
in the master plan's phase section and the `index.md` row, as part of
this planning commit, so later steps need not redo them.

1. **Adoption needs more than the master plan lists.** It names the
   scope config, `REVIEWS.md` and the `prune-reviews` workflow. The
   procedure also needs these, all missing from andris today:
   - `tools/review-tracking.sh` and `tools/ci-prune-reviews.sh`,
     byte-identical to development's
     `templates/review-tracking/`. andris's `tools/` holds only the
     release scripts.
   - A `.gitignore` change. Line 13 ignores `.vscode/` whole, which
     git cannot re-include under, so it becomes `.vscode/*` plus
     exceptions.
   - Excludes for the review marks on `trailing-whitespace` and
     `end-of-file-fixer`, at `.pre-commit-config.yaml:30-31`. Both
     rewrite files, which is what the `review-marks-pre-commit`
     audit checks.
   - CI path filtering for review-only changes (finding 3).
   - A push token for the prune bot (finding 4).
2. **More audits apply than the two the master plan names.** The
   scope config's arrival opts andris into five checks:
   `review-coverage`, `review-scope-completeness`,
   `review-tracking-templates`, `review-marks-pre-commit` and
   `review-scope-ci`. The list comes from `scripts/audit/checks/review.py`
   and `ci_workflows.py` on development's `main`. `review-coverage`
   fails whenever five or more in-scope files need review. So it will
   open an issue on the first audit after adoption, and keep it open
   until the review is nearly done. The master plan's success criteria
   speak of "the documented human-review exemption". No exemption
   mechanism exists in the audit. The issue is the work queue, and it
   closes itself once the backlog drops below five.
3. **CI would run the full merge tier for every review session.**
   - `ci.yml`'s `check_paths` filters only on `pull_request`, and
     only `docs/**` and `LICENSE` (`ci.yml:59-72`). Its header says
     the merge tier always runs.
   - The `Develop branch` ruleset requires a pull request, the merge
     queue and `Can see status`, `Can enqueue` and `Can merge`. So a
     review session's pull request would build the wheel and run the
     end-to-end and kerbside lanes.
   - Ryll runs `check_paths` on `merge_group` too, comparing
     against `github.event.merge_group.base_sha`, and gates its merge
     tier on the result (`ryll/.github/workflows/ci.yml:43-90`). Its
     comment explains why the base must be the payload's and not the
     branch.
   - `codeql-analysis.yml` already skips the review state, so it
     needs no change. `secret-scan.yml` must keep reading the review
     notes, and is untouched.
4. **The prune bot needs `DEPENDENCIES_TOKEN`.** andris's ruleset
   requires pull requests, and GitHub will not accept the Actions app
   as a bypass actor. So, as on ryll, the bot pushes with
   `DEPENDENCIES_TOKEN`. Both repositories' rulesets let the same team
   bypass (actor 11722172). Ryll has that secret and andris does not:
   andris has no repository secrets at all. The template README warns
   against putting the token at the organisation level.
5. **Ryll lands review sessions as pull requests** from a `reviews`
   branch (#570, #600, #610), signed with gitsign from the review
   account. Daily prunes arrive as shakenfist-bot commits titled
   `Prune and import review marks.`. Andris follows the same shape.
6. **The tooling can serve files in reading order.** A scope config
   with `order = 'listed'` makes `next` offer the first unread file
   in `include` order, "for reading a codebase to learn it, where
   the order is the point". No adopted repository sets it yet. See
   `load_next_order` and `listed_position` in development's
   `scripts/review-tracking.py`, and "Session discipline" in
   `docs/code-review-tracking.md`.
7. **Three tracked files are generated:**
   - `andris/src/platform/x11/evdev_names.rs`;
   - `andris/src/platform/x11/testdata/evdev.names`;
   - `andris/src/platform/x11/testdata/xfree86.names`.

   All three come from `scripts/gen-evdev-names.py`, and each says so
   in its header. Like ryll's protoc output, they are excluded, and
   the generator is in scope.
8. **The size of the review.** Outside `docs/plans/`, 130 files are
   tracked, about 50,000 lines. With the excludes in decision 2, and
   the four files this phase adds, about 125 files are in scope:

   | Part | Lines |
   |------|-------|
   | `andris/src/` | 26,700 |
   | `andris/tests/` | 8,100 |
   | Rust files over 1,200 lines | `server/updates/stream.rs` (1,681), `x11/clipboard.rs` (1,362), `synthetic/mod.rs` (1,309), `platform/damage.rs` (1,296) |

   This is weeks of sessions, not days. The phase's Definition of
   done has to survive prunes made meanwhile (decision 8).
9. **Some in-scope files are template copies.** For example
   `PLAN-TEMPLATE.md`, `PUSH-AUDIT.md`,
   `.github/actions/cargo-cache/action.yml` and the three
   review-tracking files. Where a copy is byte-identical to a file
   development has reviewed, the bot's `import` credits it, and the
   operator never sees it in `next`. That is by design, and it
   shrinks the backlog a little.

Nothing else in the master plan's section was wrong. The phase 11
section and the `index.md` row now name the full adoption, the five
audits and the review-coverage issue's real behaviour.

### Phase 10's completion check

Phase 10 merged as #28 (`553ecfb`), #29 (`c0617e9`, the 0.1.0
release) and #49 (`c36e87a`, the acceptance record). All three are
on `develop`. #49 carries the soak record and the Definition of done
evidence, and Future work carries the one gap the soak left: a soak
through kerbside. No local worktree or branch is left for the
phase. The remote branches `x11-desktop-phase-09-audio`,
`-phase-10-packaging` and `-phase-10-acceptance` remain on GitHub.
Deleting them is harmless and is left to the operator. This
planning commit's close-out marks phase 10 Complete in the master
plan.

## Decisions

1. **One andris pull request, one development pull request, then
   the sessions.** The andris pull request carries everything except
   review marks:
   - the reading guide;
   - the scope config;
   - the template copies;
   - the `.gitignore`, pre-commit and CI changes;
   - the empty `REVIEWS.md`.

   The development pull request adds andris to the adopters table.
   The two are independent. The prune workflow works without the
   table row, and the `review-tracking-templates` audit finds
   adopters by their scope config. The operator's sessions follow,
   one pull request each. The last session's pull request also
   records this plan's Result (decision 8). There is no separate
   acceptance pull request.

2. **Scope is everything a person wrote, tests included.**
   - **Include**, in decision 3's order, then these globs:
     - `*.rs`, `*.sh`, `*.py`, `*.md`, `*.yml`, `*.yaml`, `*.toml`
       and `*.json`;
     - `*.service`, `*.gitignore`, `*Makefile` and `*Dockerfile`;
     - `.devcontainer/*`.
   - **Exclude**, each with a comment giving the reason, as ryll's
     config does:
     - `docs/plans/*`;
     - `*Cargo.lock`;
     - `LICENSE`;
     - `.github/exported-config/*`, the export workflow's snapshot of
       GitHub settings;
     - `evdev_names.rs` and `testdata/*.names` (finding 7).

   Tests stay in. Much of andris's behaviour is pinned only by its
   tests, and `#[cfg(test)]` modules cannot be separated from the
   files they sit in anyway. `.devcontainer/wheel/requirements.txt`
   is reached by `.devcontainer/*`. Renovate edits its hashes, but a
   human chose the packages in it.

3. **`next` offers files in reading order: `order = 'listed'`.** This
   is the decision most likely to be argued with. The fleet default is
   random, and no adopted repository uses `listed`.
   - **Why random is the default:** it spreads attention over a
     backlog for an audit.
   - **Why this phase differs:** the master plan says why the operator
     asked for it. They want "to read andris and learn how it works".
     Random order would hand them `heat.rs` before the server it
     serves.
   - **How the order is set:** the `include` list names files
     explicitly, in the reading guide's order, and ends with the globs
     of decision 2. A file added later lands under a glob, at the end
     of the queue, so scope stays complete. A renamed file falls to
     the same globs, so a stale explicit entry costs only its position.
   - **The cost:** the list is long, 50 to 60 entries, and has to stay
     in step with the guide. A check in the Definition of done keeps
     the two agreeing.
   - **How to revert:** if the operator prefers random order once the
     first pass is done, delete the `order` line.

   The order, which step 1 may refine and must then mirror in the
   scope config:

   | # | Part | Files |
   |---|------|-------|
   | 1 | Orientation | `README.md`, `ARCHITECTURE.md`, `docs/index.md`, `docs/running.md`, `docs/xrdp.md`, `docs/reading-guide.md` |
   | 2 | The binary | `andris/src/main.rs`, `lib.rs`, `config.rs`, `install.rs` |
   | 3 | The server core | `andris/src/server/mod.rs`, `link.rs`, `tls.rs`, `session.rs`, `channel.rs`, `opcodes.rs`, `main_channel.rs`, `stats.rs` |
   | 4 | Simple channels | `inputs.rs`, `keymap.rs`, `cursor.rs` |
   | 5 | The display channel | `display.rs`, then `updates/mod.rs`, `shadow.rs`, `heat.rs`, `encode.rs`, `lossy.rs`, `stream.rs`, `test_support.rs` |
   | 6 | Agent and sound | `agent.rs`, `playback.rs` |
   | 7 | The platform seam | `andris/src/platform/mod.rs`, `damage.rs`, `audio.rs`, `synthetic/mod.rs`, `synthetic/tone.rs` |
   | 8 | X11 | `platform/x11/mod.rs`, `checks.rs`, `shm.rs`, `capture.rs`, `damage.rs`, `cursor.rs`, `input.rs`, `keymap.rs`, `randr.rs`, `clipboard.rs` |
   | 9 | PulseAudio | `platform/pulse/ffi.rs`, `pulse/mod.rs` |
   | 10 | Tests | `andris/src/test_logs.rs`, `andris/tests/support/*`, then `andris/tests/*` |
   | 11 | Build and packaging | `Cargo.toml`, `andris/Cargo.toml`, `andris/pyproject.toml`, `Makefile`, `packaging/systemd/andris@.service`, `deny.toml`, `clippy.toml`, `.cargo/audit.toml`, `renovate.json`, `.gitignore`, `.pre-commit-config.yaml`, `.devcontainer/*` |
   | 12 | Scripts | `scripts/*`, `tools/*` |
   | 13 | CI | `.github/*` |
   | 14 | The rest | `docs/development.md`, `docs/releasing.md`, `AGENTS.md`, `PLAN-TEMPLATE.md`, `PUSH-AUDIT.md` |

   Rows 10 to 14 may stay globs. Within a glob `next` goes in path
   order, which is good enough for tests and scripts.

4. **The reading guide is `docs/reading-guide.md`.**
   - **Structure:** one `##` section per row of decision 3's table,
     and one `###` heading per explicitly listed file, whose text is
     the file's path in backticks. Each entry says what the file is
     for, what it owns, what to look at closely (the invariants, the
     subtle parts, where it talks to ryll's crates or to X), and what
     to read beside it.
   - **No plan references:** it says nothing about plan phases. The
     master plan's success criteria ask `docs/` to describe andris
     "without reference to plan phases".
   - **Links in:** `docs/index.md` and one line in `ARCHITECTURE.md`'s
     documentation map.
   - **README:** untouched. It is a pitch, and a reading guide is not
     part of the install story.

5. **CI skips review-only changes in both tiers, as ryll's does.**
   - `check_paths` runs on `pull_request` and `merge_group`, with
     ryll's `base: ${{ github.event.merge_group.base_sha || 'develop'
     }}` and its comment.
   - Its filter adds `!REVIEWS.md`, `!.vscode/*.weaudit` and
     `!.vscode/*.weaudit-shas.json` to the existing `!docs/**` and
     `!LICENSE`.
   - The merge-tier jobs gain the same `code_changed` condition as
     the smoke tier.
   - `.vscode/review-scope.toml` is not filtered. It is policy, and
     `review-scope-ci` checks that a change to it reaches the
     automated reviewer.

   This also stops the merge tier running for documentation-only
   pull requests, which today it does. That is deliberate: neither
   docs nor review state can change what the merge tier builds, and
   one filter shared by both tiers is simpler than two. The header
   comment changes to say so.

6. **The prune bot pushes with `DEPENDENCIES_TOKEN`, a repository
   secret the operator adds** (finding 4). It is never an
   organisation secret. Until it exists, `prune-reviews` falls back to
   `GITHUB_TOKEN`, and its push is refused by the ruleset. Step 7
   checks the first run after the secret lands.

7. **Review sessions follow development's session discipline, from
   the review account.**
   - Each session works on a `reviews` branch in the review account's
     clone. It runs `prune`, then `next` until the session ends, then
     `stamp` and a signed commit. It lands as a pull request through
     the merge queue.
   - The management session never marks, stamps or commits review
     state: those are the operator's attestation. When asked, it
     answers questions about a file.
   - A finding goes in a weAudit note and an issue. The fix, if any,
     is its own pull request.

8. **Done means every in-scope file is reviewed on one commit of
   `develop`, measured once.** The review takes weeks, and Renovate
   and other work will change reviewed files meanwhile, so prunes will
   take marks back. The phase is done when
   `./tools/review-tracking.sh status` on a `develop` commit reports
   no file needing review. The Result records that commit, the date,
   the in-scope count, the number of sessions and the issues filed.
   Coverage after that is the steady state that `prune-reviews` and
   the `review-coverage` audit maintain.

## Execution

Implementation is done by sub-agents briefed from this plan. The
management session reviews each diff and makes each commit.

- Every commit must leave `pre-commit run --all-files` clean. Any
  commit that touches CI or code must also leave `make lint` passing.
- **No sub-agent creates, edits or stamps review marks**, meaning
  `.vscode/*.weaudit` or `.vscode/*.weaudit-shas.json`. Only `regen`
  may write `REVIEWS.md`.
- **No sub-agent modifies the development checkout** at
  `../development`. It is the operator's, and is behind `main`. For
  development's files and scripts, use a fresh clone of
  `https://github.com/shakenfist/development` in a scratch directory,
  on `main`, and point the wrapper at it with
  `SHAKENFIST_DEVELOPMENT`.
- **No sub-agent changes GitHub settings or secrets.**
- Run every make target as `env -u DISPLAY -u XAUTHORITY -u
  PULSE_SERVER -u PULSE_COOKIE make ...`. As in phase 10, no
  sub-agent connects to the host's `:10` session or sound server.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Complete | Commit phase 10's close-out and this plan. |
| 1 | high | opus | none | Complete | Write `docs/reading-guide.md`. See brief 1. |
| 2 | medium | sonnet | none | Complete | The adoption files, the scope config and `REVIEWS.md`. See brief 2. |
| 3 | medium | opus | none | Complete | CI skips review-only changes in both tiers. See brief 3. |
| 4 | low | sonnet | none | Complete | Add andris to development's adopters table, in a development worktree. See brief 4. |
| 5 | low | operator | none | In progress | Add `DEPENDENCIES_TOKEN` to andris, and ready the review account's clone. See brief 5. |
| 6 | medium | management | none | Not started | Push audit, then both pull requests. See brief 6. |
| 7 | low | management | none | Not started | After the merge, check that the prune bot and the audits work. See brief 7. |
| 8 | high | operator | none | Not started | The review sessions. See brief 8. |
| 9 | low | management | none | Not started | Record the Result in the last session's pull request. See brief 9. |

Step 1 comes first, because step 2's `include` order mirrors the
guide's. Step 3 may run beside steps 1 and 2. Step 4 and step 5 may
happen at any time before step 6 and step 7 respectively.

### Execution notes

- 2026-10-28 AEDT. The back brief was approved as written, and the
  operator gave permission to push and open both pull requests, the
  andris one and development's adopters-table one.
- Steps 1 and 4 ran in parallel: step 4 writes only to its own
  development worktree, so it cannot collide with the guide. Step 2
  followed step 1 as planned, and step 3 ran after step 2 rather
  than beside them, so that each step's `pre-commit` run had the
  tree to itself.
- The gate after step 1 was held. The operator approved the guide's
  order unchanged, including its departures from decision 3's table:
  the opcodes before the channel, the encoder, lossy and heat modules
  before the stream, capture before shm, the PulseAudio module before
  its FFI, and `audit.toml` beside `deny.toml`. Rows 10 and 11 became
  one heading per file.
- Brief 3 said the smoke tier gates its jobs with `!cancelled()`. It
  does not, and nor does ryll. The step dropped it from the merge
  tier, because "Can merge" fails on a failed `check_paths` either
  way, and the brief is corrected.
- Step 5: the operator reported `DEPENDENCIES_TOKEN` added, but on
  2026-10-28 at 16:07 AEDT the andris repository, Dependabot and
  `release` environment secret listings were all empty, while ryll's
  shows it. Nothing needs it before the merge; step 7 checks again
  before dispatching the prune bot.
- 2026-10-31 (AEDT): `DEPENDENCIES_TOKEN` became a repository secret
  here on 2026-10-29, and the prune bot's pushes then landed as
  `shakenfist-bot`. From the review-tracking re-sync pull request
  (#65) on, the `prune-reviews` workflow no longer reads that secret:
  it pushes as the `shakenfist-review-marks` GitHub App, minting a
  token from `REVIEW_MARKS_APP_KEY` in the `review-marks` environment
  (branch policy `develop` only), and the App is a bypass actor on
  the develop ruleset. Steps 4 to 6 and brief 5 below describe the
  token setup as it was when this phase was planned; the setup that
  matters now is step 7 of "Adopting a repository" in
  shakenfist/development's `docs/code-review-tracking.md`, and it
  was done for andris on 2026-10-30 by `PLAN-bot-bypass-identity.md`
  there.

### Brief 1: the reading guide

Write `docs/reading-guide.md`, in the structure of decision 4 and the
order of decision 3. Read every file you describe. The guide is only
worth having if each entry is true of the code as it stands.

- Begin with a short introduction:
  - who the guide is for: someone who knows Rust and roughly what
    SPICE is, and has not read andris;
  - how to use it with the review tooling: `./tools/review-tracking.sh
    next` offers files in this order;
  - one paragraph on how a frame and a key press travel through
    andris, naming the files involved, so that the order makes sense.
- **Per file, a few sentences to two short paragraphs:**
  - what it does;
  - its main types and functions;
  - what to look at closely;
  - what to read beside it.

  "What to look at" is the valuable part. Examples are the
  link-and-ticket check in `link.rs`, the single-client replacement
  in `session.rs`, the flow control in the display channel, the
  stream creation and teardown rules in `stream.rs`, the X request
  stall handling, the `unsafe` in `shm.rs` and `pulse/ffi.rs`, and
  `install.rs`'s refusal paths. Find the real ones. Do not trust this
  list.
- Where a file talks to ryll's crates (`shakenfist-spice-protocol`,
  `shakenfist-spice-compression`), name the types it uses. Readers
  will want to open them.
- You may refine the order in decision 3 if the code argues for it,
  for example if a file is unreadable before another. Say what you
  changed and why in your report, because step 2 mirrors it.
- Rows 10 to 14 may be shorter: a paragraph per directory, with the
  few files worth a note named.
- **Constraints:**
  - No reference to plan phases, phase numbers or `docs/plans/`
    files.
  - British spelling, matching the rest of `docs/`.
  - Wrap at 72 to 76 columns like the other pages.
  - `pre-commit run --all-files` must pass. There is a markdown link
    checker and a mermaid lint, so check any diagram you add.
- Add the guide to `docs/index.md`'s Documentation Index, and give it
  one line in `ARCHITECTURE.md` where that file maps the
  documentation (around lines 6 to 8).

Commit subject: `Add a reading guide to andris's code.`

### Brief 2: the adoption files

Follow steps 1, 4, 5, 6 and 7 of "Adopting a repository" in a fresh
clone of shakenfist/development on `main`. Read that section in full
before starting.

- **`.gitignore`:** replace `.vscode/` with `.vscode/*` and the
  three exceptions, `!.vscode/*.weaudit`,
  `!.vscode/*.weaudit-shas.json` and `!.vscode/review-scope.toml`.
  The imports file is covered by the second. Do not un-ignore the
  day log.
- **`.pre-commit-config.yaml`:** add `exclude: ^\.vscode/.*\.weaudit`
  to `trailing-whitespace` and `end-of-file-fixer` (lines 30 and 31),
  and nowhere else. Scope it to those hooks, never to the top level,
  so that gitleaks and the bidi check still read review notes.
- **Template copies:** copy `review-tracking.sh` to
  `tools/review-tracking.sh`, `ci-prune-reviews.sh` to
  `tools/ci-prune-reviews.sh`, and `prune-reviews.yml` to
  `.github/workflows/prune-reviews.yml`, all from
  `templates/review-tracking/`, byte for byte. Then run the
  `git hash-object` loop from development's
  `.claude/skills/review-tracking-adoption/SKILL.md` and paste its
  output in your report. If shellcheck or actionlint in andris's
  pre-commit objects to a copy, stop and report. Do not edit the
  copy.
- **`.vscode/review-scope.toml`:**
  - Model the header comment on ryll's `.vscode/review-scope.toml`:
    the reasons for what is in, and one comment per exclude saying
    why.
  - Set `order = 'listed'`, with a comment saying why (decision 3)
    and pointing at `docs/reading-guide.md`.
  - Make `include` the explicit paths in the reading guide's final
    order (take it from step 1's committed guide, not from
    decision 3's table), followed by decision 2's globs.
  - Make `exclude` decision 2's list.
- **Check the scope:**
  - `SHAKENFIST_DEVELOPMENT=<clone> ./tools/review-tracking.sh
    scope-orphans` must report no orphans.
  - `./tools/review-tracking.sh next --no-open` must offer
    `README.md`, the first entry.
  - Paste both outputs.
- **`REVIEWS.md`:** generate it with `./tools/review-tracking.sh
  regen`. There are no pre-existing marks, so step 6's bootstrap is a
  no-op. Commit the generated file as it comes, and never hand-edit
  it.
- **`docs/development.md`:** add a short section, "Reviewing
  andris". It should say:
  - that andris is in development's review tracking, with a link to
    `docs/code-review-tracking.md`;
  - that `next` follows `docs/reading-guide.md`'s order;
  - that sessions land as pull requests from a `reviews` branch;
  - that the `review-coverage` audit keeps an issue open while five
    or more in-scope files need review.

  No plan references.

Commit subject: `Adopt andris into review tracking.`

### Brief 3: CI skips review-only changes

Edit `.github/workflows/ci.yml` to decision 5. Mirror ryll's
`check_paths`, in `../ryll/.github/workflows/ci.yml` lines 40 to 90,
and its merge-tier conditions. Read ryll's comments, which carry the
reasons, before writing andris's.

- `check_paths` runs for `pull_request` and `merge_group`, and not
  for `workflow_dispatch`, which falls back to `code_changed=true`.
  Ryll's job checks the repository out on every event but dispatch.
  Find out from `dorny/paths-filter`'s documentation whether a
  `merge_group` diff needs it, say so in a comment, and copy ryll's
  `base`.
- Add the three review-state exclusions to the filter, and keep
  `!docs/**` and `!LICENSE`. Do not add `.vscode/review-scope.toml`.
- Gate every merge-tier job on `needs.check_paths.outputs.code_changed
  != 'false'`, as the smoke tier already is. The gates' `jq` must
  still treat a skipped job as success. Check that `Can merge` cannot
  go green when `check_paths` itself failed.
- Rewrite the header comment (lines 12 to 21) to say that both tiers
  skip documentation and review state, and why.
- Leave `secret-scan.yml`, `codeql-analysis.yml` and the
  `automated_reviewer` job alone.
- `pre-commit run --all-files` runs actionlint and must pass.

Commit subject: `Skip review-only changes in both CI tiers.`

### Brief 4: development's adopters table

In a new worktree of `../development` from an up-to-date `main`, on a
branch `andris-review-tracking`:

- Add a row to the "Projects using these templates" table in
  `templates/review-tracking/README.md`: andris, `develop`,
  `DEPENDENCIES_TOKEN`. Keep the table sorted.
- `git grep -n kerbside-client` on `main` finds any other adopter
  lists. Add andris where the list is of adopters, and nowhere that
  is generated (`docs/audits/compliance.md` is).
- Run development's pre-commit and its `scripts/tests` for the
  review-tracking README, if any test reads the table.

Commit subject: `Add andris to the review-tracking adopters.`

### Brief 5: the operator's setup

The operator, not an agent:

- Adds a **repository** secret `DEPENDENCIES_TOKEN` to
  shakenfist/andris, with the same token ryll uses. Do not make it an
  organisation secret.
- Clones andris into the review account, with gitsign signing
  configured as in "Commit signing" in
  `docs/code-review-tracking.md`, and weAudit installed in that
  account's VSCode.

### Brief 6: push audit and pull requests

Run `PUSH-AUDIT.md` over `develop...HEAD`. The diff is mostly prose,
configuration and CI, so expect the documentation and security
reviewers to matter most. The security reviewer should read the CI
change for any way a code change could be skipped. Then:

- push the branch and open the andris pull request;
- push brief 4's branch and open the development pull request;
- ask the operator to merge them, development's first or in either
  order.

This plan step is the operator's authority to open both.

### Brief 7: after the merge

Once the andris pull request has merged and brief 5's secret exists:

- Dispatch `prune-reviews.yml` on `develop` (`gh workflow run
  prune-reviews.yml`), and check that it succeeds. With no marks, it
  may commit nothing, or an import of template copies (finding 9).
  Read `.vscode/imports.weaudit-shas.json` if it lands.
- After the next daily consistency audit, check development's
  `docs/audits/compliance.md` for andris:
  - `review-scope-completeness`, `review-tracking-templates`,
    `review-marks-pre-commit` and `review-scope-ci` should be
    compliant;
  - `review-coverage` should fail, with an issue open on andris
    (finding 2).

  Record each result in the Result section.
- Record the phase's blocker with plan-blockers as a decision: "the
  operator's review sessions until `status` reports nothing to
  review".

### Brief 8: the review sessions (operator)

Use decision 7's discipline, at whatever pace suits. Each session:

1. `git pull` on a clean `develop`, `git switch -c reviews` (or
   reset an existing one), `./tools/review-tracking.sh prune`.
2. `./tools/review-tracking.sh next`, read the file, and mark it
   reviewed in weAudit. Attach any notes or findings. Repeat.
3. `./tools/review-tracking.sh stamp`, then `git add .vscode/*.weaudit*
   REVIEWS.md`, then a signed `git commit`. Push, and open a pull
   request.
4. File an issue for each finding that needs a change.

Any Claude session may be asked about a file, and answers from the
code and the reading guide. If a session finds the guide wrong, fix
it in its own pull request. That edit makes `docs/reading-guide.md`
need review again, which is expected.

### Brief 9: the Result

When `./tools/review-tracking.sh status` on `develop` reports nothing
to review, record these in this plan's Result section, in the last
session's pull request or a small one after it:

- the commit and date;
- the in-scope count, and how many files were imported rather than
  read;
- the number of sessions and their pull requests;
- the issues filed from findings, and which are fixed;
- the `review-coverage` issue's closure.

Mark the master plan's success criterion about review marks as met,
and the phase's last step Complete.

## Risks and mitigations

- **The reading guide could be wrong.** A confident, wrong guide is
  worse than none, because the operator reads it beside the code.
  Step 1 uses opus at high effort and reads every file. The management
  session spot-checks five entries against the code before committing,
  choosing the "what to look at" claims. The operator's sessions are
  the final check, and brief 8 fixes what they find.
- **The CI change could let a real change skip the merge tier.**
  - The filter is an every-pattern exclusion list. A file outside the
    list counts as code, and the output fails open.
  - Brief 6's security reviewer reads it for skips.
  - The Definition of done asks the andris pull request itself to run
    the merge tier, which it touches code to do. The first review
    session's pull request must then skip it.
- **The listed order and the guide may drift.** The Definition of
  done's agreement check runs in the andris pull request. Brief 8 has
  guide fixes made in their own pull requests, where the same check can
  be re-run.
- **Prunes take marks back during a long review** (decision 8). The
  stable code is read first and the churny configuration last
  (rows 11 to 13), so fewer marks are lost. The Definition of done is
  measured once, on one commit.
- **The prune bot's first push may be refused** if the secret or the
  bypass is wrong. Brief 7 dispatches it and reads the run.
- **A template copy may trip andris's pre-commit.** Brief 2 stops
  rather than editing a byte-identical copy. The fix then belongs in
  development's template.

## Definition of done

- **The copies are identical.** Brief 2's `git hash-object` loop
  prints `ok` for all three files, against development's `main` on the
  day of the pull request.
- **Scope is complete.** `./tools/review-tracking.sh scope-orphans`
  reports no orphans. development's `review-scope-completeness` column
  reads compliant for andris.
- **The review state is committable, and the day log is not.**
  `git check-ignore .vscode/mikal.weaudit
  .vscode/mikal.weaudit-shas.json .vscode/imports.weaudit-shas.json
  .vscode/review-scope.toml` prints nothing. `git check-ignore
  .vscode/.weauditdaylog` prints its path.
- **The guide and the queue agree.** This prints `ok`:

  ```
  python3 -I - <<'EOF'
  import re, subprocess, tomllib
  include = tomllib.load(open('.vscode/review-scope.toml', 'rb'))['include']
  listed = [p for p in include if '*' not in p]
  guide = re.findall(r'^###\s+`([^`]+)`', open('docs/reading-guide.md').read(), re.M)
  tracked = set(subprocess.run(['git', 'ls-files'], capture_output=True, text=True).stdout.split())
  assert set(listed) <= tracked, sorted(set(listed) - tracked)
  assert guide == listed, (guide, listed)
  print('ok', len(listed))
  EOF
  ```

- **The guide cites no plans.** `grep -n -iE 'phase|docs/plans|PLAN-'
  docs/reading-guide.md` finds nothing, apart from any ordinary use of
  "phase" that the management session reads and accepts.
- **CI skips review state, and only review state and docs.**
  - The andris pull request, which changes CI, runs both tiers in
    full.
  - The first review session's pull request shows the smoke and merge
    tier jobs skipped, and `Can enqueue` and `Can merge` green.
  - `secret-scan` ran on both.
- **The bot works.** A `prune-reviews` run on `develop` after the
  secret was added succeeded, and is linked in the Result.
- **Development lists andris.** Its adopters table has the row, and
  `review-tracking-templates` reads compliant for andris.
- **Every in-scope file was reviewed** (decision 8). The Result names
  a `develop` commit on which `./tools/review-tracking.sh status`
  reported nothing needing review, and the `review-coverage` issue is
  closed.
- **Findings are tracked.** Every finding that needs a change has an
  issue, linked from the Result.

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.

**Gate after step 1.** The reading guide fixes the order that step
2's scope config takes, and the order that the operator will read in
for weeks. Show the operator the guide's section and file order
before step 2 starts, and change it if they ask.

## Result

Not started.
