# Phase 1: Build and CI scaffold

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: high. Most of the work follows ryll, but the two-stage
CI and the merge queue have correctness rules (gates that tolerate
skipped jobs, concurrency that survives queue rebuilds) that are easy
to copy subtly wrong.

## Scope

In:

- a Cargo workspace with one zero-dependency binary crate, and the
  fleet's unwrap lint;
- the Docker devcontainer and the `Makefile` that runs cargo inside it,
  with offline compilation;
- the Rust pre-commit hook;
- `deny.toml`, and the weekly supply-chain workflow;
- two-stage CI with gates and the automated reviewer, replacing phase
  0's lint-only `ci.yml`;
- CodeQL, the re-review and retest workflows, and the `RETEST_WORKFLOW`
  variable;
- the merge queue and required status checks on `develop`;
- `docs/development.md`, plus the AGENTS.md, ARCHITECTURE.md and
  `docs/index.md` changes that the existence of code requires.

Out:

- any SPICE, X11 or ryll-crate dependency, which starts in phase 4;
- system libraries in the devcontainer for those (libxcb and the
  like), which are added with the code that needs them;
- packaging, releases and crates.io publishing, which are phase 10;
- fuzzing, which arrives when there is a parser to fuzz.

## What the survey found

Checked on 2026-10-08 against andris `e89539f`, ryll `4d5f9e9`,
development `066d639`.

- **Phase 0 is complete.** Its commits are on `develop`
  (`60c0c4d..e89539f`) and development's `main` (`066d639`), and no
  branch or worktree is left over. The work it deferred (CodeQL, CI
  review automation, the merge queue) is assigned to this phase in its
  Situation table. It is closed out in this branch's first commit.
- **A dry run of the audit with a stub workspace** (a scratch clone of
  andris with a one-file binary crate) gave 30 pass, 3 fail:
  - `rust-unwrap-lint`, which a `Cargo.toml` makes applicable. It needs
    `[workspace.lints.clippy] unwrap_used = "warn"`, a root
    `clippy.toml` with `allow-unwrap-in-tests = true`, and
    `[lints] workspace = true` in every member;
  - `github-security` and `ci-review-automation`, as expected.

  No other check changed state, so those three are the audit's whole
  to-do list for this phase. Two more apply once the queue exists:
  `merge-queue-config` reads the live ruleset, and
  `merge-group-cancellation` applies to any workflow with a
  `merge_group:` trigger.
- **The master plan's phase 1 section was stale** in three places,
  and all three are corrected at source in this branch:
  - it said to *add* pre-commit with shellcheck, which phase 0
    already did; only the Rust hook is new;
  - it did not mention CodeQL, the CI review automation or the merge
    queue, which phase 0 deferred to here;
  - it spoke of a "human-review exemption" in the audit result. There
    is none: those checks report not applicable.
- **Ryll's machinery, and what transfers:**
  - `Makefile`: `DOCKER_RUN` / `DOCKER_RUN_OFFLINE`, the `CARGO_CACHE`
    override with its empty-value guard, and `fetch` as the only
    networked compile-free target. This transfers almost whole; the
    QEMU, fuzz, packaging and macOS targets do not.
  - `.devcontainer/Dockerfile`: Debian base and rustup stable. Ryll's
    GUI, audio and Windows packages do not transfer.
  - `scripts/check-rust.sh`: the pre-commit entry point; it runs
    rustfmt and clippy in the devcontainer image.
  - `.github/actions/cargo-cache`: restores `.cargo-cache` after
    checkout, keyed on `Cargo.lock`.
  - `.github/workflows/ci.yml`: `check_paths` uses dorny/paths-filter,
    and smoke jobs run on `pull_request`. The merge tier runs on
    `merge_group` and the reviewer through
    `shakenfist/actions/.github/workflows/pr-auto-review.yml@main`.
    The three gates `Can see status`, `Can enqueue` and `Can merge`
    each map every dependency to success-or-skipped with jq.
  - `.github/workflows/supply-chain.yml`: a weekly cron.
  - Ryll's `codeql-analysis.yml` is byte-identical to development's
    template.
- **The `Develop branch` ruleset** on andris currently has deletion,
  non-fast-forward and pull-request rules, with the bypass team ryll
  uses (`11722172`). Ryll's ruleset adds a `merge_queue` rule
  (`max_entries_to_build` 1, `min_entries_to_merge` 1, ALLGREEN, MERGE,
  5, 360) and `required_status_checks` for the three gates
  (`integration_id` 15368).
- **Ryll sets `RETEST_WORKFLOW=ci.yml`** as a repository variable;
  andris has no variables yet.
- **Unknown until a pull request runs:** whether the org secrets the
  reviewer workflow uses are granted to andris. The token used for
  this survey cannot read org secret or runner-group settings. Runner
  access is already confirmed: phase 0's `Secret scan` run succeeded.

## Decisions

1. **One crate, `andris`, a binary with no dependencies.** It prints
   its name and version and exits, and carries one unit test. This
   gives every tool (clippy, tests, cargo-deny, cargo-audit, CodeQL)
   something real to run against without committing to any design
   that phase 4 owns. The master plan defaults to splitting crates
   only when a second backend exists, so the workspace starts with
   one member.
2. **Edition 2024, `rust-version = "1.88"`.** This is the decision a
   reviewer is most likely to question, because ryll is on 2021.
   Editions are per crate and interoperate freely, so andris depending
   on ryll's crates is unaffected. A new crate has no migration cost,
   and starting on 2021 would only create a migration to do later. The
   MSRV matches ryll's, because andris will compile ryll's crates and
   cannot have a lower floor than they do.
3. **Offline compilation from the start.** The `Makefile` keeps
   ryll's split. `make fetch` is the only target with network access
   to the cargo cache. Every compiling target runs in Docker with
   `--network none` and the cache mounted read-only. It costs nothing
   while there are no dependencies, and it is far easier to keep than
   to retrofit.
4. **Lint is `pre-commit run --all-files`, in one CI job.** Andris
   already runs every hook in CI, as development does. The Rust hook
   joins them, so CI and a developer's commit run the same checks and
   cannot drift apart. The cost is that the lint job needs a Docker
   runner (`[self-hosted, vm, debian-13-docker, l]`), because
   `check-rust.sh` uses the devcontainer image. Ryll's separate
   skillsaw and shellcheck jobs are not copied; pre-commit covers
   both.
5. **The merge tier rebuilds and retests the merge result.** Andris
   is Linux-only, so ryll's cross-platform matrix has no counterpart.
   The merge tier runs `make release test` on the merge-group commit.
   The smoke tier tested the pull request head; this tests what would
   actually land. It is cheap now and becomes the home of the Xvfb
   end-to-end lane in phase 4.
6. **gitleaks stays in `secret-scan.yml`.** Phase 0's workflow already
   satisfies the secret-scanning audit and runs on pushes to
   `develop`, which `ci.yml` does not. It is not duplicated into
   `ci.yml`, and it is not a gate dependency: like ryll's content
   scanners it runs on documentation-only changes too.
7. **`deny.toml` follows ryll's policy, narrowed to andris's
   targets.** The graph is `x86_64-unknown-linux-gnu` and
   `aarch64-unknown-linux-gnu`. The licence allow-list is ryll's, and
   there are no advisory ignores. `allow-git` lists
   `https://github.com/shakenfist/ryll`, because phase 3 may pin ryll
   by git revision as kerbside does. `.cargo/audit.toml` is not
   created: it exists in ryll only to mirror advisory ignores, and
   andris has none.
8. **The weekly supply-chain cron arrives now.** It does almost
   nothing with zero dependencies. Adding it with the rest of the
   supply-chain policy means it cannot be forgotten when phase 4
   brings the first dependencies.
9. **CodeQL is the template, verbatim.** The template autodetects
   languages. If autodetection does not pick up Rust, or the
   autobuild fails, step 6 pins `languages: rust` with
   `build-mode: none`. That is a documented deviation, as kerbside
   carries.
10. **The phase 1 pull request goes through its own merge queue.** The
    ruleset change (merge queue plus required gates) is applied once
    the pull request's smoke tier is green and before it merges, so
    the queue is proven by the change that introduces it. If the
    `merge_group` run never starts or the gates misreport, the bypass
    team can merge directly and the rule can be removed. That is
    recorded as a finding, not papered over.
11. **The README does not change.** There is still nothing to install
    or run; how to build belongs in `docs/development.md`.

## Execution

Each step is one commit on `x11-desktop-phase-01-scaffold`, made by
the management session after review. Every commit must leave
`make lint` and `make test` passing once step 2 has landed.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 1 | low | sonnet | none | Not started | Cargo workspace and stub crate. See brief 1. |
| 2 | high | opus | none | Not started | Devcontainer and Makefile. See brief 2. |
| 3 | medium | sonnet | none | Not started | Rust pre-commit hook. See brief 3. |
| 4 | medium | sonnet | none | Not started | Supply-chain policy. See brief 4. |
| 5 | high | opus | none | Not started | Two-stage CI. See brief 5. |
| 6 | medium | sonnet | none | Not started | CodeQL and CI review automation. See brief 6. |
| 7 | medium | sonnet | none | Not started | Documentation. See brief 7. |
| 8 | medium | management | none | Not started | Audit, push, pull request, queue. See brief 8. |

All paths below are relative to the worktree,
`/srv/kasm_profiles/mikal/vscode/src/shakenfist/andris-wt-p01`; ryll
is at `../ryll` and development at `../development`.

**Brief 1: Cargo workspace and stub crate.**

- Root `Cargo.toml`: `[workspace]` with `resolver = "3"` and
  `members = ["andris"]`.
- `[workspace.package]`: `version = "0.0.1"`, `edition = "2024"`,
  `rust-version = "1.88"`, `license = "Apache-2.0"`,
  `repository = "https://github.com/shakenfist/andris"`, and authors
  as in ryll's root `Cargo.toml`.
- `[workspace.lints.clippy] unwrap_used = "warn"`.
- Copy ryll's `[profile.dev] debug = "line-tables-only"`, with its
  comment trimmed to the reason.
- Root `clippy.toml` containing `allow-unwrap-in-tests = true`.
- `andris/Cargo.toml`: every package field inherits the workspace
  (`version.workspace = true`, and so on), plus
  `description = "A SPICE server that exports an existing X11
  desktop."` and `[lints] workspace = true`.
- `andris/src/main.rs`: print
  `andris <CARGO_PKG_VERSION>` (via `env!`) to stdout and exit 0.
  Put the string in a small function so a unit test can assert on
  it, and add that test.
- Commit `Cargo.lock`, because this is a binary.
- Do not add dependencies, and do not build on the host: step 2
  provides the build.

Commit subject: `Add the Cargo workspace and a stub binary.`

**Brief 2: devcontainer and Makefile.** Read ryll's `Makefile`,
`.devcontainer/Dockerfile`, `.devcontainer/devcontainer.json` and
`docs/ci.md` first. The comments in the `Makefile` explain the
security model; keep that reasoning, trimmed to what andris has.

- `.devcontainer/Dockerfile`: Debian devcontainer base,
  `build-essential` and `pkg-config` only, rustup stable with
  `rustfmt` and `clippy` installed under `/build` with `umask 0000`,
  exactly as ryll does. Install no other system packages. A comment
  says that system libraries arrive with the code that needs them.
- `.devcontainer/devcontainer.json`: as ryll's, renamed.
- `Makefile`, with image `andris-dev` and these targets:
  - `help` and `devcontainer`;
  - `ensure-cache`, including the root-container ownership fix;
  - `fetch`, the networked target with a writable cache, and `lock`;
  - `build`, `release`, `test` and `lint`, all offline and depending
    on `fetch`; `lint` is `cargo fmt --all -- --check` plus
    `cargo clippy --workspace --all-targets -- -D warnings`;
  - `lint-fix`;
  - `clean`, which removes the cache only when it is inside the
    checkout, as ryll's does.

  Keep `CARGO_CACHE ?=` with its empty-value `$(error ...)` guard,
  plus `DOCKER_BASE_ARGS`, `CACHE_MOUNTS`/`CACHE_MOUNTS_RO`,
  `DOCKER_RUN` and `DOCKER_RUN_OFFLINE`, and the `CARGO_BUILD_JOBS`
  passthrough. Drop everything QEMU-, fuzz-, packaging-, release-,
  macOS- and Windows-related, and `RYLL_GIT_SHA`.
- `.gitignore` already ignores `/target/` and `/.cargo-cache/`;
  confirm it.
- Verify with `make lint`, `make test`, `make release`, and
  `docker run --rm --network none ...`. That last one shows the
  offline targets really have no network: an offline `make build`
  must succeed after `make fetch`.

Commit subject: `Add the devcontainer and Makefile.`

**Brief 3: the Rust pre-commit hook.**

- Port `../ryll/scripts/check-rust.sh` to `scripts/check-rust.sh`. It
  takes `check` or `fix`, builds the `andris-dev` image if it is
  missing, and runs rustfmt and clippy (`-D warnings`) over the
  workspace in the image, with the same cache-ownership fix.
- Make it reuse the `Makefile`'s offline behaviour, either by calling
  `make lint` / `make lint-fix` or by matching its flags. Prefer
  calling `make`, so the two cannot drift; say in a comment why.
- Add the local `rust-check` hook to `.pre-commit-config.yaml`, as
  ryll's is: `files: \.rs$|Cargo\.(toml|lock)$`,
  `pass_filenames: false`.
- The script must pass shellcheck, which pre-commit already runs over
  `scripts/`. Run `pre-commit run --all-files`.

Commit subject: `Run rustfmt and clippy from pre-commit.`

**Brief 4: supply-chain policy.**

- `deny.toml` from ryll's, with these changes:
  - `[graph] targets` is `x86_64-unknown-linux-gnu` and
    `aarch64-unknown-linux-gnu`;
  - `ignore = []`;
  - no licence `exceptions`;
  - `allow-git = ["https://github.com/shakenfist/ryll"]`.

  Keep the licence allow-list, `confidence-threshold`, `[bans]` and
  the rest of `[sources]`. Keep the comments that explain policy;
  drop those about ryll's own crates.
- `.github/workflows/supply-chain.yml` from ryll's: a weekly cron
  running cargo audit and cargo deny, with ryll-specific comments
  removed. It must keep a top-level `permissions:` block and
  self-hosted runners with a size label.
- Verify that `cargo deny check` passes, using the cargo-deny-action
  container or a devcontainer with cargo-deny installed ad hoc. Do
  not add cargo-deny to the image for this.

Commit subject: `Add the supply-chain policy.`

**Brief 5: two-stage CI.** Replace phase 0's lint-only
`.github/workflows/ci.yml`.

Read these first:

- ryll's `.github/workflows/ci.yml`, all of it;
- ryll's `.github/actions/cargo-cache/action.yml`;
- the audit specifications in `../development/docs/audits/`:
  `merge-group-cancellation.md`, `expensive-lane-path-filter.md`,
  `workflow-standards.md`, `ci-review-automation.md`,
  `self-hosted-runners.md` and `llm-context-lint-ci.md`.

Then:

- Copy `.github/actions/cargo-cache/action.yml` verbatim, apart from
  the comment wording.
- `ci.yml` has these jobs:
  - `check_paths` (static runner, dorny/paths-filter). Code changed
    unless every file matches `docs/**`, `**/*.md` or `LICENSE`.
    Mirror ryll's filter and comments; andris has no `REVIEWS.md` or
    `.vscode` review files.
  - **Smoke tier** (`pull_request`, `workflow_dispatch`; skipped on
    `merge_group` and when `code_changed == 'false'`):
    - `lint`: `pre-commit run --all-files` in a venv, on
      `[self-hosted, vm, debian-13-docker, l]` with cargo-cache;
    - `build-test`: `make build test`, same runner, with cargo-cache;
    - `cargo-deny`: cargo-deny-action, as ryll's;
    - `cargo-audit`: rustsec/audit-check, as ryll's.
  - **Merge tier** (`merge_group` only): `merge-build`, running
    `make release test` on the merge commit.
  - `automated_reviewer`: needs the four smoke jobs and calls
    `shakenfist/actions/.github/workflows/pr-auto-review.yml@main`
    with the permissions block ryll uses. Never `secrets: inherit`.
  - The gates, copied from ryll with their needs lists adjusted:
    - `can_see_status`;
    - `can_enqueue`, which needs `check_paths` and the four smoke
      jobs;
    - `can_merge`, which needs `check_paths` and `merge-build`.
- Every job on a self-hosted vm runner names a size label.
- Concurrency must satisfy `merge-group-cancellation.md`: on
  `merge_group`, the group key must not use `github.ref` or
  `github.sha`. Copy the exact expression ryll uses for its merge
  tier, and explain it in a comment.
- Job and workflow display names are English sentences.
- The top-level `permissions: contents: read` stays, with per-job
  overrides as in ryll.
- Validate with actionlint: install it in the pre-commit venv, or
  add the actionlint pre-commit hook if ryll or development uses
  one. Then run the audit's workflow checks:
  `python3 ../development/scripts/audit-check.py --repo-path .
  --repo-name andris --github-org shakenfist`.

Commit subject: `Replace the lint workflow with two-stage CI.`

**Brief 6: CodeQL and CI review automation.**

- Copy `../development/templates/codeql/codeql-analysis.yml`
  verbatim to `.github/workflows/codeql-analysis.yml`. Read the
  template's README first. Do not pin languages pre-emptively; the
  management session decides that from the pull request's first run.
- Copy `../development/templates/ci-review-automation/pr-re-review.yml`
  and `pr-retest.yml` verbatim into `.github/workflows/`, after
  reading that template's README.
- Confirm that `ci.yml` has a `workflow_dispatch:` trigger, because
  `pr-retest.yml` dispatches it.
- Do not set the repository variable; the management session does
  that.
- Re-run the audit. github-security and ci-review-automation must now
  pass.

Commit subject: `Add CodeQL and the review bot workflows.`

**Brief 7: documentation.** Follow
`../development/templates/shared-blocks/llm-doc-discipline.md`. Docs
describe what exists, with no phase numbers outside `docs/plans/`.

- New `docs/development.md` covers:
  - prerequisites (Docker only);
  - the `make` targets, and why compiling targets are offline;
  - pre-commit;
  - the two CI tiers, the gates and the merge queue;
  - the supply-chain checks and the weekly cron;
  - how to retest or re-review a pull request with the bot phrases.

  Use ryll's `docs/development.md` and `docs/ci.md` as models,
  trimmed to andris.
- `docs/index.md`: link it.
- `AGENTS.md`:
  - the opening no longer says there is no Cargo workspace,
    `Makefile` or CI. Say there is a stub binary and the build and
    CI scaffold;
  - the cargo convention drops "when the Cargo workspace exists";
  - add one line on `unwrap_used`: tests may unwrap, production code
    may not;
  - nothing else changes.
- `ARCHITECTURE.md`: the "repository today" table gains the
  workspace and the `andris` crate, and its prose stops saying there
  is no build CI. The intended-shape section is unchanged.
- Do not change `README.md`.
- Run `pre-commit run --all-files` and the audit.

Commit subject: `Document building, testing and CI.`

**Brief 8: audit, push, pull request, queue** (management session).

1. Run `PUSH-AUDIT.md` over the branch diff against `develop`.
2. Run the audit locally. Every failure must be fixed except
   `merge-queue-config`, which reads the live ruleset.
3. Set `gh variable set RETEST_WORKFLOW --repo shakenfist/andris
   --body ci.yml`.
4. Push the branch, and ask the operator before opening the pull
   request.
5. When the pull request runs, check that:
   - every smoke job and `Can enqueue` passes;
   - CodeQL succeeds, or apply the pin in decision 9;
   - the automated reviewer comments. If it fails on missing
     secrets, the operator grants andris access to them, because
     that needs org admin.
6. Once the smoke tier is green, apply the ruleset change from
   decision 10. Update the `Develop branch` ruleset (id `24700229`)
   to add ryll's `merge_queue` rule and the `required_status_checks`
   rule for the three gates. Then enqueue the pull request, confirm
   that `merge-build` and `Can merge` run on `merge_group`, and that
   it merges.
7. Re-run the audit against `develop`, and record the result in this
   file.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| The reviewer workflow's org secrets are not granted to new repositories | Visible on the first pull request run. The operator grants access, and the management session does not work around it, for example by removing the reviewer from the gate. |
| A required check that never reports (path-filtered at the trigger, or skipped wrongly) blocks every merge | The gates use ryll's success-or-skipped jq. Brief 5 forbids trigger-level filtering on `ci.yml`. Step 8 proves the queue on this very pull request, and the bypass team is the fallback. |
| The merge-group concurrency key makes a superseding merge group cancel the wrong run, or none | Brief 5 copies ryll's expression and the audit's merge-group-cancellation check reads it. The management session confirms it in step 8, with the audit passing. |
| CodeQL autodetection ignores Rust, or autobuild fails | Decision 9 sets out the pin. It is decided from evidence on the first run, not guessed. |
| `make lint` in CI cannot reach Docker | The lint and build jobs use `debian-13-docker` runners, as ryll's do. Phase 0 proved the static and `vm, debian-13, s` labels serve andris; the docker label is proven by this pull request. |
| The offline build silently has network access | Brief 2's verification runs an offline build after `make fetch`. Reviewers check for `--network none` in every compiling target. |

## Definition of done

Each item can be checked with the command given:

- **Clean build and lint.** In a fresh clone,
  `make lint && make test && make release` passes with Docker as the
  only host dependency.
- **The binary runs.**
  `docker run --rm -v "$PWD":/w -w /w andris-dev target/release/andris`
  prints `andris 0.0.1`.
- **No audit failures.** On `develop` after the merge, this prints
  nothing:

  ```
  python3 ../development/scripts/audit-check.py --repo-path .
  --repo-name andris --github-org shakenfist | jq -r
  '.checks[] | select(.status=="fail" or .status=="error") | .id'
  ```

- **The merge queue is configured.**
  `gh api repos/shakenfist/andris/rules/branches/develop` includes a
  `merge_queue` rule with `max_entries_to_build` 1 and
  `min_entries_to_merge` 1, and a `required_status_checks` rule
  naming `Can see status`, `Can enqueue` and `Can merge`.
- **This pull request went through the queue.** Its `merge_group`
  run shows `merge-build` and `Can merge` succeeded.
- **The retest variable is set.** `gh variable list -R
  shakenfist/andris` shows `RETEST_WORKFLOW ci.yml`.
- **The reviewer ran.** The automated reviewer left a review comment
  on this pull request.
- **AGENTS.md is current.**
  `grep -n "no Cargo workspace" AGENTS.md ARCHITECTURE.md` finds
  nothing.

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan. Gate: decision 10 (putting this
pull request through a merge queue it introduces) changes the
repository's merge rules, so confirm it with the operator before
step 8.6, not only at the start.
