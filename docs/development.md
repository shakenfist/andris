# Development

How to build, test and contribute to andris, and how its continuous
integration works. For the conventions AI coding assistants follow
see
[AGENTS.md](https://github.com/shakenfist/andris/blob/develop/AGENTS.md).

Andris today is a build and CI scaffold around a stub binary: the
workspace has one crate, `andris`, which prints its name and
version. What andris is meant to become is set out in the
[master plan](plans/PLAN-x11-desktop.md).

## Prerequisites

Docker. Nothing else is needed on the host, and in particular no
Rust toolchain: every cargo invocation runs inside a devcontainer
built from
[`.devcontainer/Dockerfile`](https://github.com/shakenfist/andris/blob/develop/.devcontainer/Dockerfile)
(Debian with a C toolchain, `pkg-config`, and stable Rust with
rustfmt and clippy). Run `make` targets as an ordinary user in the
docker group; the container runs as your uid and gid.

## Make targets

```bash
make build      # debug build of the whole workspace
make release    # release build
make test       # cargo test across the workspace
make lint       # cargo fmt --check, then clippy with -D warnings
make lint-fix   # rustfmt and clippy --fix
make fetch      # pre-download crates into the cargo cache
make lock       # refresh Cargo.lock after editing a Cargo.toml
make clean      # remove target/ and an in-checkout cargo cache
make help       # list the targets
```

`make devcontainer` builds the `andris-dev` image; the other targets
depend on it.

### Why compiling targets are offline

`build`, `release`, `test`, `lint` and `lint-fix` first run
`make fetch`. `fetch` and `lock` are the only targets allowed
network access, and neither compiles anything. The compiling targets
then run with
`--network none` and the cargo cache mounted read-only, with
`--frozen`. A dependency's `build.rs` runs arbitrary code at compile
time, so this stops a compromised crate from reaching the network
or poisoning the cache for later runs.

The consequence is that `Cargo.lock` is never written by a build.
After adding or bumping a dependency, run `make lock`, which is the
one target that updates the lockfile (it still runs no build
script). Without it the next build fails at `fetch` with "the lock
file needs to be updated".

The cargo download cache lives in `.cargo-cache` in the checkout.
Override `CARGO_CACHE` to move it, for example to a directory a CI
runner keeps between jobs. `make clean` only deletes the cache when
it is inside the checkout. Set `CARGO_BUILD_JOBS` in your
environment to bound build parallelism on a small machine.

## Lints and style

`make lint` treats every clippy warning as an error. `clippy::unwrap_used` is on for
the workspace: production code must not call `unwrap()`, while
tests may (`clippy.toml` sets `allow-unwrap-in-tests`).

Dependencies are checked by `cargo deny` against
[`deny.toml`](https://github.com/shakenfist/andris/blob/develop/deny.toml):
a licence allowlist, yanked crates denied, and crates.io as the only
registry. Git dependencies are denied except for the ryll
repository. Add a licence to the allowlist only when a dependency
needs it, and give every advisory ignore a URL and a rationale.

## Pre-commit

```bash
pre-commit run --all-files
```

Run this before proposing a commit; CI runs the same hooks. The
hooks are:

- **rustfmt and clippy**, through
  [`scripts/check-rust.sh`](https://github.com/shakenfist/andris/blob/develop/scripts/check-rust.sh),
  which calls `make lint` so the hook and the Makefile cannot drift.
  It runs only when a `.rs` file or a `Cargo.toml`/`Cargo.lock`
  changes.
- **actionlint** over the workflows, using
  [`.github/actionlint.yaml`](https://github.com/shakenfist/andris/blob/develop/.github/actionlint.yaml)
  to declare the self-hosted runner labels in use.
- **shellcheck** over `scripts/` and `tools/`.
- **gitleaks** for secrets.
- **skillsaw**, which lints the agent context such as `AGENTS.md`.
- Whitespace, end-of-file, YAML syntax and merge-conflict checks.

Install pre-commit in a virtualenv, since Debian-based systems
refuse a system-wide pip install.

## Continuous integration

CI runs in two tiers, and `develop` is protected by a merge queue.
A pull request gets the cheap checks on self-hosted runners. What
would actually land is then rebuilt and retested once, as the merge
queue's candidate commit. The workflow is
[`ci.yml`](https://github.com/shakenfist/andris/blob/develop/.github/workflows/ci.yml).

### The smoke tier

Runs on `pull_request` and on `workflow_dispatch`, and gates the
`Can enqueue` status:

| Job | What it does |
|-----|--------------|
| `Lint` | `pre-commit run --all-files`, so every hook above |
| `Build and test` | `make build test` |
| `cargo audit` | RustSec advisory check |
| `cargo deny` | Licence, ban, source and advisory policy |

An `Automated reviewer` job runs after these four through the shared
workflow in shakenfist/actions and posts a review on the pull
request. It does not gate anything.

### The merge tier

Runs only on `merge_group` and gates `Can merge`. The single job,
`Build and test the merge commit`, runs `make release test` against
the commit the queue has built from the pull request and the current
tip of `develop`. The smoke tier's jobs do not run on `merge_group`.

### Gates and path filtering

Three jobs are the required status checks on `develop`:

| Gate | Runs on | Passes when |
|------|---------|-------------|
| `Can see status` | every event | always; it only proves status reporting works |
| `Can enqueue` | not `merge_group` | every smoke job succeeded or was skipped |
| `Can merge` | `merge_group` | the merge-tier job succeeded or was skipped |

A `Check paths` job skips the lint, build and supply-chain jobs
when a pull request changes only `docs/**` and `LICENSE`. Other
Markdown, such as `AGENTS.md`, is not skipped, since the `skillsaw`
hook lints it. The filter is applied per job and not with a
`paths-ignore:` on the trigger: a required check in a workflow that
never started never reports, and would block the merge forever.
The gates treat a skipped job as success for the same reason.

### Other workflows

| Workflow | When | What it does |
|----------|------|--------------|
| `secret-scan.yml` | every pull request, pushes to `develop` | gitleaks over full history; not path-filtered, because a credential lands in documentation as easily as in code |
| `supply-chain.yml` | Mondays 09:00 UTC, and on demand | `cargo audit` and `cargo deny` against `develop`, so a newly published advisory is noticed without waiting for a pull request |
| `codeql-analysis.yml` | pull requests, pushes to `main` and `develop`, Tuesdays 17:00 UTC | CodeQL; skipped for documentation-only changes |
| `renovate.yml` | hourly | self-hosted Renovate dependency updates |
| `export-repo-config.yml` | nightly | exports repository settings into the repository |
| `pr-re-review.yml`, `pr-retest.yml` | comments on a pull request | the bot phrases below |

The CI jobs that compile restore the cargo download cache through
the
[`cargo-cache`](https://github.com/shakenfist/andris/blob/develop/.github/actions/cargo-cache/action.yml)
action. It must come after `actions/checkout`, whose clean step
would otherwise delete `.cargo-cache`.

### The merge queue

Pull requests land through the merge queue, not by a direct merge.
The `Develop branch` ruleset requires a merge queue and requires the
three gates, `Can see status`, `Can enqueue` and `Can merge`, to
pass. The sequence is:

1. The pull request's smoke tier passes, so `Can enqueue` is green.
2. The pull request is added to the queue.
3. The queue builds a merge commit and runs `ci.yml` on
   `merge_group`. `Build and test the merge commit` and `Can merge`
   run against it.
4. If they pass, the queue merges. If not, the pull request is
   ejected and the failure is in the `merge_group` run's log.

A failure on the merge tier is not necessarily the pull request's
fault: a flaky runner or an advisory published since the smoke tier
ran can cause one. Re-enqueue before assuming otherwise.

## Retesting and re-reviewing a pull request

Comment on the pull request with one of these phrases. The comment
must contain the phrase `@shakenfist-bot` followed by the words:

| Comment | Effect |
|---------|--------|
| `@shakenfist-bot please retest` | dispatches `ci.yml` on the pull request's branch, which runs the smoke tier again; the repository variable `RETEST_WORKFLOW` names the workflow |
| `@shakenfist-bot please re-review` | runs the automated reviewer again over the pull request, even if it has already reviewed this head |

Only a user with write or admin permission on the repository may use
them, the comment must not come from a bot, and the pull request
must come from a branch in this repository: requests on fork pull
requests are refused. The bot reacts to the comment with a rocket
when it accepts it. A retest posts a comment linking the workflow
runs, or a comment saying it could not dispatch.

When a job fails for a reason unrelated to the change, such as a
runner outage, re-running the failed job from the Actions tab is
cheaper than a full retest.
