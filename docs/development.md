# Development

How to build, test and contribute to andris, and how its continuous
integration works. For the conventions AI coding assistants follow
see
[AGENTS.md](https://github.com/shakenfist/andris/blob/develop/AGENTS.md).

The workspace has one crate, `andris`, a library and the binary
that runs it. How to run andris is in [running.md](running.md), and
what it is meant to become is set out in the
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
make ryll-headless  # build ryll's headless client at the pinned revision
make e2e        # run andris against Xvfb, driven by ryll
make e2e-xrdp   # run andris against a real xrdp session, in a container
make lint-fix   # rustfmt and clippy --fix
make fetch      # pre-download crates into the cargo cache
make lock       # refresh Cargo.lock after editing a Cargo.toml
make clean      # remove target/ and an in-checkout cargo cache
make help       # list the targets
```

`make devcontainer` builds the `andris-dev` image; the other targets
depend on it.

### The end-to-end test

`make e2e` runs andris against a real X server and a real client.
Inside the devcontainer, with no network and a read-only checkout,
[`scripts/e2e.sh`](https://github.com/shakenfist/andris/blob/develop/scripts/e2e.sh)
starts Xvfb and runs the X11 backend's integration test,
`andris/tests/x11.rs`, against its bare root window: it draws a
rectangle, waits for the damage, and captures it through both MIT-SHM
and `GetImage`. It then detaches andris's shared segment from a second
connection, finding it through X-Resource 1.2 as another client
could, and checks that capture recovers: first with a fresh segment,
then, after a second detach, through `GetImage`. A second test resizes
the display through andris's RandR display control: smaller, wider
than Xvfb allows (clamped to its widest), narrower and taller, and back
to the start, each reaching damage subscribers as a resize; then larger
than Xvfb allows, which is refused. It checks that andris leaves none
of the modes it creates behind. Four more check the clipboard with
`xclip`: copies on the display reach andris, including a copy of the
same text again, text over a megabyte (which arrives in pieces, by
INCR), and text an owner offers only as Latin-1; text andris is given
pastes on the display, as UTF-8, Latin-1 where it can be, and in pieces
when large, while a requestor that stops taking pieces holds up no
other paste; another program that takes the clipboard from andris is
followed, and andris stops serving; and an owner that never answers is
given up on after andris's 10-second timeout, while andris still takes
and serves text meanwhile. The tests take turns with the display, which
they read from `ANDRIS_TEST_DISPLAY`. When the variable is unset, as
under `make test`, each prints that it is skipping and passes. The
script then starts a full-screen window that logs the keys it
receives, starts the debug andris against that display, and connects
ryll's headless client through a `.vv` file. Last, it asserts, over
ryll's control socket:

- **frames**: a frame of the window arrived;
- **keys**: two keys reached the window;
- **idle silence**: with nothing changing, once the desktop has settled (the
  driver waits for quiet first), no draw arrives for two seconds, since andris sends frames only on damage;
- **a small change**: a key makes the window repaint a 64x64 square,
  which must reach the client in fewer draws than there are 64-row
  bands in the screen;
- **the clipboard**: text set with ryll's `clipboard_set` pastes on the
  display with `xclip`, and text copied there with `xclip` reaches
  ryll's `clipboard_get`, each way at three sizes: within one 2048-byte
  agent chunk, over it, and over the megabyte above which X clients
  send text in pieces, by INCR. A paste of the largest by hand checks
  that andris served it by INCR;
- **resizes**: ryll's `resize` to a smaller size and back each reach
  ryll as a new surface; two sizes larger than Xvfb's `-screen` size
  are refused, andris logs the refusal once, and the surface keeps its
  size. The resize checks read andris's log, and ryll's, whose debug
  lines record each agent reply.

It also checks ryll's log for all four channels, and stops ryll with
SIGINT and andris with SIGTERM; both must exit 0. Last, it starts a
second andris against the same display and kills Xvfb under it: that
andris must exit 1 within ten seconds, logging that the desktop has
gone and why. It fails if the first andris logs an `ERROR`, and prints
every log on failure.

It needs nothing on the host beyond Docker: Xvfb and Python are in
the devcontainer image, and `make ryll-headless`, which `make e2e`
depends on, builds ryll with `--no-default-features` (no GUI, audio or
capture) at the revision `Cargo.lock` pins for
`shakenfist-spice-protocol`. That build needs the network, installs
into `ryll/<revision>/` under the cargo cache, and is skipped when the
binary for that revision is already there. Moving the ryll pin builds
ryll afresh.

### The xrdp test bed

`make e2e-xrdp` runs andris against a real xrdp session. It builds
a Docker image (`andris-xrdp-testbed`, on top of `andris-dev`) with
xrdp, xorgxrdp and a FreeRDP client, and in a throwaway container,
[`scripts/xrdp-testbed/run.sh`](https://github.com/shakenfist/andris/blob/develop/scripts/xrdp-testbed/run.sh)
starts xrdp and connects FreeRDP, so that xrdp starts a real Xorg
session, on display `:50`. Andris and ryll's headless client then
share that session with the RDP client, and
[`scripts/e2e-xrdp-drive.py`](https://github.com/shakenfist/andris/blob/develop/scripts/e2e-xrdp-drive.py)
checks, after a smoke test, that:

- keys typed alternately over RDP and SPICE arrive once each, in order;
- modifiers held over one protocol apply to keys from the other;
- lock keys set over RDP and by ryll behave as
  [xrdp.md](xrdp.md#rdp-and-spice-together) says;
- an RDP resize reaches ryll as a new surface;
- an RDP disconnect and reconnect leave andris and ryll connected;
- an RDP reconnect with another layout changes what SPICE keys type;
- two lines copied on FreeRDP's display reach ryll with the LF line
  ends they started with, though RDP carries them as CRLF, and two
  lines set in ryll paste on FreeRDP's display;
- the clipboard does not loop when the RDP client and ryll share one
  clipboard, as they do on a Mac. A stand-in for that shared clipboard,
  [`scripts/e2e-pasteboard.py`](https://github.com/shakenfist/andris/blob/develop/scripts/e2e-pasteboard.py),
  joins FreeRDP's display and ryll, so each copy can come back into the
  session through the other client; after a copy of two lines in the
  session and one on FreeRDP's display, andris must log one crossing
  for each, and none in the five seconds after, though RDP brings each
  back with CRLF line ends;
- a resize from ryll is refused, since xorgxrdp does not let clients
  resize it: andris logs it once, ryll keeps the session's size, and an
  RDP resize afterwards still reaches ryll;
- ending the session makes andris exit with status 1.

Every clipboard check copies text not used before, because FreeRDP
and xrdp keep what they last fetched.

The container has no network, and its own `/tmp`, so no X server or
xrdp session on the host can be reached from it. Never aim a test that
resizes, logs out or types into a session at a real one: the test bed
exists so that nobody has to.

It needs Docker, plus the network the first time, to build the image
and install its packages; the image is rebuilt only when its inputs
change. It is not part of CI: what it guards is xrdp's behaviour, and
a remote-desktop client driven through `xdotool` on a shared runner is
the sort of check that flakes. A warm run, with the image and binaries
current, takes about 45 seconds.

### The JPEG corpus measurement

`andris/tests/jpeg_corpus.rs` is an ignored test that measures real
screen content, to justify the constants of the JPEG classifier in
`server/updates/encode.rs`. It asserts nothing about andris's behaviour; it
prints, per image and rectangle size, the LZ4 and JPEG sizes and encode
rates, and the share of rectangles and of area the classifier would
send as JPEG.

It reads every file in `target/corpus` (or the directory in
`$ANDRIS_JPEG_CORPUS`): raw packed 32-bit B, G, R, X pixels, named
`<name>_<W>x<H>.bgrx`. Put the files there, then run it in release,
inside the devcontainer, so the timings mean something:

```bash
cargo test --release --frozen --test jpeg_corpus -- --ignored --nocapture
```

`make test` skips it. A capture of a real desktop must never be
committed: it holds whatever was on the screen. `target/` is
untracked, and the test prints numbers, never pixels.

### Why compiling targets are offline

`build`, `release`, `test`, `lint` and `lint-fix` first run
`make fetch`. `fetch` and `lock` are the only targets allowed
network access, and neither compiles anything. A dependency's
`build.rs` runs arbitrary code at compile time, so the compiling
targets pass `--frozen` and run inside a sandbox:

- `--network none`, so a compromised crate cannot reach the network
  to fetch a payload or send anything out.
- The cargo cache mounted read-only, so a build script cannot poison
  the downloaded crates for later runs.
- For `build`, `release`, `test` and `lint`, the checkout mounted
  read-only with only `target/` writable. The default cargo cache
  lives inside the checkout, so a writable checkout would let a
  build script rewrite `.cargo-cache` (which CI's cache action then
  saves for every later run) or plant a `core.fsmonitor` or
  `core.hooksPath` setting in `.git/config`, which would run on the
  host the next time anything there runs git. Sources, `Cargo.lock`,
  `.git` and the cache are all out of reach; `target/` is not, so its
  contents are only as trustworthy as the code compiled into them.
  The Makefile refuses to run if `target` is a symlink, since
  mounting one writable would undo the read-only checkout.

`lint-fix` is the exception to the last point. rustfmt and
`clippy --fix` rewrite sources, so it mounts the checkout writable,
and clippy compiles the dependency tree to do its work: build
scripts run with `.git` and an in-checkout cache writable. It is
still offline with a read-only cache mount, but run it only on code
you trust. CI never runs it.

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

`make lint` treats every clippy warning as an error.
`clippy::unwrap_used` is on for the workspace: production code must
not call `unwrap()`, while tests may (`clippy.toml` sets
`allow-unwrap-in-tests`).

Dependencies are checked by `cargo deny` against
[`deny.toml`](https://github.com/shakenfist/andris/blob/develop/deny.toml):
a licence allowlist, yanked crates denied, and crates.io as the only
registry. Git dependencies are denied except for the ryll
repository. Add a licence to the allowlist only when a dependency
needs it, and give every advisory ignore a URL and a rationale.
`cargo audit` runs too and does not read `deny.toml`, so
[`.cargo/audit.toml`](https://github.com/shakenfist/andris/blob/develop/.cargo/audit.toml)
repeats the same advisory ignores; change both together.

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

None of the four runs for a pull request from a fork, since each
would execute the fork's code on runners that hold the CI mesh key:
they are skipped, which `Can enqueue` accepts, so push a fork's
branch to this repository to test it. The merge tier cannot be
guarded this way, so only enqueue a fork pull request after
reviewing its code.

### The merge tier

Runs only on `merge_group` and gates `Can merge`. Two jobs run.
`Build and test the merge commit` runs `make release test` against
the commit the queue has built from the pull request and the current
tip of `develop`. The smoke tier's jobs do not run on `merge_group`.

The other, `End to end`, runs `make e2e` on `merge_group` and on
`workflow_dispatch`, and `Can merge` requires it too. It is in the
merge tier because building ryll would add over a minute to every
push; its cargo cache has a key prefix of its own, so that only a new
ryll pin pays for the build, and so that the offline jobs never
restore a cache the networked ryll build wrote. A retest
of a branch dispatches it, so it can be tried before the branch is
enqueued; nothing gates on that run.

### Gates and path filtering

Three jobs are the required status checks on `develop`:

| Gate | Runs on | Passes when |
|------|---------|-------------|
| `Can see status` | every event | always; it only proves status reporting works |
| `Can enqueue` | not `merge_group` | every smoke job succeeded or was skipped |
| `Can merge` | `merge_group` | both merge-tier jobs succeeded or were skipped |

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
   `merge_group`. `Build and test the merge commit`, `End to end`
   and `Can merge` run against it.
4. If they pass, the queue merges. If not, the pull request is
   ejected and the failure is in the `merge_group` run's log.

A failure on the merge tier is not necessarily the pull request's
fault: a flaky runner can cause one, so re-enqueue before assuming
otherwise. A semantic conflict with something merged to `develop`
since the smoke tier ran fails it too, and needs a rebase.

## Retesting and re-reviewing a pull request

Comment on the pull request with one of these phrases. The comment
must contain the phrase `@shakenfist-bot` followed by the words:

| Comment | Effect |
|---------|--------|
| `@shakenfist-bot please retest` | dispatches `ci.yml` on the pull request's branch, which runs the smoke tier and the end-to-end test again; the repository variable `RETEST_WORKFLOW` names the workflow |
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
