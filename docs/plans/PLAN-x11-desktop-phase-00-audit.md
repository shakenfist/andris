# Phase 0: Join the consistency audit

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: medium. The work follows templates that already
exist; the judgement is in what the seeded documents say about a
repository that has a plan and no code.

## Situation

`shakenfist/andris` was created on 2026-10-08 (public, Apache-2.0
intended, default branch `develop`, delete-branch-on-merge on). Its
only commit is the master plan.

Running development's audit against the clone
(`python3 scripts/audit-check.py --repo-path ../andris --repo-name
andris --github-org shakenfist`) before seeding gave 9 failures, 4
passes and 47 not applicable:

| Check | Failure | Handled in |
|-------|---------|------------|
| llm-tooling | No `AGENTS.md`, `ARCHITECTURE.md` | Step 1 |
| renovate | No `renovate.json`, `.github/workflows/renovate.yml` | Step 2 |
| pre-commit-config | No `.pre-commit-config.yaml` | Step 2 |
| export-repo-config | No `.github/workflows/export-repo-config.yml` | Step 2 |
| default-branch-naming | Repository did not exist | Repo creation |
| delete-branch-on-merge | Repository did not exist | Repo creation |
| merge-queue-config | Repository did not exist | Repo creation; the queue itself is phase 1 |
| github-security | No CodeQL workflow | Phase 1: CodeQL has nothing to analyse until there is Rust |
| ci-review-automation | No re-review/retest workflows, no reviewer job | Phase 1: these hang off a CI workflow that does not exist yet |

Many checks are not applicable only because the files they inspect
are absent (`readme-structure`, `plan-template`, `push-audit`,
`llm-doc-structure`, `llm-context-lint`, ...). Seeding those files
brings the checks into play, so Step 3 re-runs the audit until it
settles.

Human review is out of scope: review-coverage and
review-scope-completeness apply only where `.vscode/review-scope.toml`
exists, so andris deliberately does not get the review-tracking
tooling (`.vscode/review-scope.toml`, `REVIEWS.md`,
`prune-reviews.yml`).

## Execution

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 1 | high | opus | none | Complete | Agent and human documentation. See brief 1. |
| 2 | medium | sonnet | none | Complete | Repository tooling. See brief 2. |
| 3 | medium | opus | none | Complete | Re-run the audit and close what it newly reports. See brief 3. |
| 4 | low | sonnet | none | Complete | Add andris to development's audit scope. See brief 4. |
| 5 | low | management | none | Complete | Push, apply the branch ruleset, close out. See brief 5. |

Steps 1 to 3 land directly on andris `develop`, one commit per
step: there is no CI and no ruleset yet, so a pull request would
review nothing. Step 5 applies the ruleset, after which every later
phase lands by pull request.

**Brief 1: agent and human documentation.** In
`/srv/kasm_profiles/mikal/vscode/src/shakenfist/andris`, create the
files below. Ryll (`../ryll`) is the closest sibling, being Rust and
SPICE: read its version of each file first and follow its shape.
Shared blocks (`<!-- shared-block: ... -->` through
`<!-- shared-block-end -->`) are copied **byte for byte** from
`../development/templates/shared-blocks/`, never paraphrased.
Read `../development/templates/shared-blocks/llm-doc-discipline.md`
and `readme-discipline.md` first: they define what belongs in
each file. Andris has a plan and no code, and every document
must say so honestly. Describe what exists. Point at
`docs/plans/PLAN-x11-desktop.md` for what is intended, and never
describe planned behaviour as if it exists.

- `LICENSE`: Apache-2.0, identical to ryll's.
- `.gitignore`: Rust (`/target`), editor droppings, mirroring ryll's
  where relevant.
- `README.md`: a short pitch (what andris is, who it is for, that it
  is pre-alpha with no code yet), and absolute links
  (`https://github.com/shakenfist/andris/blob/develop/...`) to
  `docs/index.md` and the master plan. No install section until
  there is something to install; say so in one line.
- `AGENTS.md`: conventions an agent cannot infer, kept short. These
  are:
  - cargo runs through the Makefile in Docker, never on the host;
  - andris only emits SPICE message types that ryll's
    `logging::message_names` models (the kerbside allowlist
    invariant from the master plan);
  - no code is translated from x11spice (GPLv3) or spice-server
    (LGPL), which are references for behaviour only;
  - plan files live in `docs/plans/` and follow `PLAN-TEMPLATE.md`;
  - Python style, if any appears, follows the fleet: single quotes
    and 120-column lines;
  - commits go through `pre-commit run --all-files`.

  Ryll's `AGENTS.md` shows the expected sections, including its
  table of protocol reference sources: reuse that table, trimmed to
  what a server needs.
- `ARCHITECTURE.md`: today's shape, which is a repository holding a
  plan, plus its relationships to ryll's shared crates and to
  kerbside. Include a short "intended shape" section that summarises
  the master plan's design commitments and links to it. Do not
  enumerate modules that do not exist.
- `docs/index.md`: the documentation index. Today it lists the plans
  index and the master plan.
- `PLAN-TEMPLATE.md`: start from ryll's (`../ryll/PLAN-TEMPLATE.md`).
  Rewrite the Prompt section and the "In this project" notes for
  andris: a server, built with `make`, with reference sources
  `/srv/src-reference/spice/{spice,spice-protocol,x11spice}` and
  ryll's crates. Keep every shared block verbatim. The
  `plan-template` audit compares those blocks with canonical, so
  diff each one against `../development/templates/shared-blocks/`
  before finishing.
- `PUSH-AUDIT.md`: start from ryll's and strip what is
  ryll-specific: the egui, client and fuzzing sections, and the
  `REVIEWS.md` handling, since andris has no review tracking. Keep
  the shared blocks verbatim and the two invocation modes.
- `docs/plans/index.md` already exists. Add the preamble paragraph
  ryll's index carries, pointing at `PLAN-TEMPLATE.md` and
  `PUSH-AUDIT.md`, without disturbing the table.

Wrap prose at about 72 columns as the existing plan does. Commit
nothing; the management session reviews and commits.

**Brief 2: repository tooling.** In the andris clone:

- Copy `../development/templates/renovate/renovate.yml` verbatim to
  `.github/workflows/renovate.yml`.
- Write `renovate.json` from that template's `renovate.json`, keeping
  the pre-commit manager enabled. Drop ryll's crate-specific groups;
  andris has no dependencies yet.
- Copy `../development/templates/export-repo-config/export-repo-config.yml`
  verbatim to `.github/workflows/export-repo-config.yml`.
- Write `.pre-commit-config.yaml` with the hooks that apply before
  there is Rust: gitleaks, skillsaw and shellcheck, at the revisions
  ryll pins, plus `pre-commit-hooks` for trailing whitespace,
  end-of-file and YAML checks. Phase 1 adds the Rust hook.
- Run `pre-commit run --all-files` (install pre-commit in a temporary
  venv if it is missing) and fix what it reports, including in Step
  1's files.

Read each template's `README.md` before copying. Commit nothing.

**Brief 3: re-run the audit.** From `../development`, run
`python3 scripts/audit-check.py --repo-path ../andris --repo-name
andris --github-org shakenfist`. Every failure must be fixed, unless
the Situation table above assigns it to phase 1. For each check that
moved from not applicable to fail, read its specification in
`../development/docs/audits/<check-id>.md` and fix the file at the
root rather than to satisfy the matcher. Repeat until stable. Report
the final result table. Commit nothing.

**Brief 4: development scope.** In a worktree of `../development`
on a new branch, add `andris` to the matrix in
`.github/workflows/consistency-audit.yml` and to the in-scope list
in `docs/audits/README.md`, alphabetically in both. Run the
repository's tests (`python3 -m unittest discover -s scripts/tests`
or as `AGENTS.md` there directs) and `pre-commit run --all-files`.
Commit nothing.

**Brief 5: push and close out** (management session):

- Commit Steps 1 to 3 and push them to andris `develop`.
- Apply a `Develop branch` ruleset mirroring ryll's (deletion,
  non-fast-forward, pull request with zero approvals), without the
  merge queue and required status checks, which arrive with CI in
  phase 1.
- Commit and push the development change to its `main`, after
  confirming with the operator.
- Re-run the audit and record the result below.
- Record the phase's commits in the master plan's `Merged` cell.

## Result

Phase 0 landed directly on andris `develop` as `60c0c4d..e89539f`
(the master plan, this plan, the documentation, and the tooling), and
on development `main` as `066d639` (andris in the audit matrix and
in-scope list). The `Develop branch` ruleset was applied afterwards,
so later phases land by pull request.

The final local audit run gave 30 pass, 2 fail, 28 not applicable
and 0 errors. The two failures are the ones the Situation table
assigns to phase 1:

- github-security: no CodeQL workflow;
- ci-review-automation: no re-review or retest workflows, and no
  reviewer job.

Departures from the briefs:

- Step 2 also added `.github/workflows/ci.yml`, a lint-only job
  running every pre-commit hook, and `.github/workflows/secret-scan.yml`
  (gitleaks, copied from development's). Two checks needed them:
  llm-context-lint-ci needs skillsaw in CI, and secret-scanning-ci
  needs gitleaks in CI. Phase 1 replaces the lint-only `ci.yml` with
  the two-stage workflow.
- Secret scanning, push protection and Dependabot security updates
  were enabled on the repository to match ryll. The github-security
  audit asks for all three.
- The first `Secret scan` run on `develop` succeeded, which confirms
  the org's self-hosted runners serve andris.
