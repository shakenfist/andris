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
(Debian with a C toolchain, `pkg-config`, `nasm`, `libpulse-dev` and
stable Rust with rustfmt and clippy). `nasm` is for openh264, which andris builds from
source for its H.264 encoder: without it the crate silently skips its
assembly, and a 1080p frame takes about 2.7 times as long to encode.
Run `make` targets as an ordinary user in the docker group; the
container runs as your uid and gid.

## Make targets

```bash
make build      # debug build of the whole workspace
make release    # release build
make test       # cargo test across the workspace (FEATURES=qemu adds the qemu platform)
make lint       # cargo fmt --check, then clippy with -D warnings, with and without the qemu feature
make ryll-headless  # build ryll's headless client at the pinned revision
make e2e        # run andris against Xvfb, driven by ryll
make e2e-xrdp   # run andris against a real xrdp session, in a container
make e2e-kerbside  # run the end-to-end checks through kerbside's demo stack
make wheel ARCH=x86_64  # build the manylinux wheel (or ARCH=aarch64), into target/wheels
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
  lines record each agent reply;
- **video**: [`scripts/e2e-video.py`](https://github.com/shakenfist/andris/blob/develop/scripts/e2e-video.py)
  paints a moving 320x240 photographic pattern at about 25 frames a
  second beside a static text area. Within 15 seconds ryll's `stats`
  show one H.264 stream decoding frames, which keeps decoding for
  three more seconds with no decode failure and no second stream, and
  andris logs a stream rectangle that covers the video and not the
  text. When the picture stops, the stream must be destroyed within two
  seconds, and within two more a screenshot of the area must match the
  picture's last frame exactly, which shows the lossless repair.
  Andris runs with debug logging for its display channel so the driver
  can summarise the client's stream reports and the bitrate changes.

After ryll stops, andris's session-end metrics line must count one
stream created and destroyed. A second ryll, started with `--no-h264`,
then watches the same picture for five seconds: it must see no stream
created, and andris's session-end line for it must count JPEG draws
and no stream.

**Audio.** Before andris starts, the script runs an unprivileged
PulseAudio daemon whose only sink, so its default, is a null sink at
48 kHz, and exports `PULSE_SERVER` naming its socket. Andris, `pacat`
and `pactl` reach it through that variable and nothing else on the
host. With it up, the PulseAudio capture's integration test
(`andris/tests/pulse.rs`) runs there too, gated by
`ANDRIS_TEST_PULSE`: it plays tones into the sink and reads them back
through andris's capture. Under `make test` the variable is unset, so
the test skips and passes. The driver then plays a 440 Hz tone for five
seconds with `pacat` and checks, through ryll's `stats` `playback`
section, that ryll saw one start and Opus at 48 kHz in two channels,
decoded at least 400 packets with no failure, and saw a stop within two
seconds of the tone ending. Andris's session-end metrics line must
count at least that many audio frames and one audio start and stop.

Ryll is built with its `audio` feature, so that it opens the playback
channel, but every ryll the script starts is given a `PULSE_SERVER`
that does not exist. The devcontainer's ALSA default is PulseAudio's
plugin, so a ryll with the script's server would play the decoded tone
into the null sink, andris would capture it again, and the stream
would never stop. Never point a ryll at the sound server andris
captures. Nothing here touches the host's sound server.

A second andris, started with `--no-audio` on another port, must give
a third ryll no playback channel: its `stats` never has a `playback`
section, and ryll's log lists no playback channel and links none.
Andris warns each time a client links the playback channel and it
cannot capture sound, so the script also fails if the first andris
logged that warning, or an `ERROR`.

It also checks ryll's log for all five channels, and stops ryll with
SIGINT and andris with SIGTERM; all three must exit 0. Last, it starts a
second andris against the same display and kills Xvfb under it: that
andris must exit 1 within ten seconds, logging that the desktop has
gone and why. It fails if the first andris logs an `ERROR`, and prints
every log on failure.

It needs nothing on the host beyond Docker: Xvfb and Python are in
the devcontainer image, and `make ryll-headless`, which `make e2e`
depends on, builds ryll with `--no-default-features --features
audio` (no GUI or capture, but with audio) at the revision
`Cargo.lock` pins for `shakenfist-spice-protocol`. That build needs the network, installs
into `ryll/<revision>/` under the cargo cache, and is skipped when the
binary for that revision is already there. Moving the ryll pin builds
ryll afresh.

### The kerbside lane

`make e2e-kerbside` runs the end-to-end checks with ryll connected
through kerbside's proxy, so that every channel andris serves, the
playback channel among them, crosses kerbside's message allowlist. It
fetches kerbside's repository at the commit `KERBSIDE_REV` names in
the Makefile, a release tagged `v$KERBSIDE_VERSION`, and runs its demo
stack (MariaDB and the released kerbside from PyPI) with
[`scripts/kerbside-lane/compose.override.yml`](https://github.com/shakenfist/andris/blob/develop/scripts/kerbside-lane/compose.override.yml)
on top. The override swaps the demo's qemu for a placeholder, points
kerbside's static source at andris with a ticket made for the run,
puts the stack on an internal network, and publishes no port.

Andris, ryll, Xvfb and PulseAudio run together in one `andris-dev`
container, in the kerbside container's network namespace. Andris then
listens on `127.0.0.1:5910`, as it must without TLS, and ryll reaches
kerbside's TLS port on the same loopback with a `.vv` file from the
demo's `get-console.sh`. [`scripts/e2e.sh`](https://github.com/shakenfist/andris/blob/develop/scripts/e2e.sh)
runs in its `E2E_VIA=kerbside` mode, which drives every channel: a
frame, keys, the clipboard both ways, one H.264 stream and one Opus
stream. Kerbside's log must then show "hypervisor connection
successful" for all five channels, and no firewall verdict, denial or
termination. The script is
[`scripts/e2e-kerbside.sh`](https://github.com/shakenfist/andris/blob/develop/scripts/e2e-kerbside.sh);
it redacts the run's secrets from the logs it keeps in
`target/kerbside-lane/`, and removes the stack with
`docker compose down -v` however it ends.

It needs Docker Compose 2.24 or newer, for `!reset`, and the network
to build the stack's images and fetch kerbside, but not to run it. The
CI runners have Docker but not Compose, so the merge-tier job first
runs `scripts/install-compose.sh`, which installs Debian's
`docker-compose` package where `docker compose` does not already run.

Bump the kerbside pin by hand, since Renovate tracks neither value:
set `KERBSIDE_VERSION` to the new release and `KERBSIDE_REV` to the
full commit its tag `v<version>` names, together. The fetch fails if
the tag names another commit. The version in
[Trying it with kerbside's demo stack](running.md#trying-it-with-kerbsides-demo-stack)
moves with them, since that recipe runs the release the lane tests.

### The wheel

`make wheel ARCH=x86_64` (or `aarch64`, on a host of that
architecture) builds andris into a `manylinux_2_28` wheel in
`target/wheels/`, with openh264 compiled in and checked for its SIMD
code. It builds in its own image, on the manylinux base pinned by
digest, with Rust and maturin at exact versions that Renovate bumps,
and with the same offline split as the targets above. The tag is
earned: maturin's policy check runs without an exception list, and
fails if the binary links a library outside the policy. The build
also fails if the binary has an RPATH or RUNPATH.
[`scripts/wheel-smoke.sh`](https://github.com/shakenfist/andris/blob/develop/scripts/wheel-smoke.sh)
then installs the wheel as pipx would in a stock `python:3-slim`
container, which has no libpulse, no X server and no systemd, and
checks that the binary runs, that `install-unit` writes a unit naming
it, and that andris serves without sound. How a release publishes
the wheels is in [releasing.md](releasing.md).

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
- a moving picture played in the session (X12), which xorgxrdp also
  sends to FreeRDP, becomes one H.264 stream that ryll decodes with no
  failure and is destroyed once the picture stops, while FreeRDP stays
  connected and logs no error;
- a 440 Hz tone played in the session with `pacat` (X13), into the
  xrdp sink, reaches ryll as Opus: it decodes packets with no failure
  and the stream stops once the tone ends, while FreeRDP stays
  connected and logs no error;
- ending the session makes andris exit with status 1.

The session has a sound server, as a Debian 13 desktop does:
PipeWire, WirePlumber and `pipewire-pulse` under `dbus-run-session`,
started in that order, with each waiting for the one before (starting
them together raced). The test bed has no desktop to run autostart
entries, so the session then runs xrdp's loader script itself, which
makes `xrdp-sink` the default. The test waits for that before it
starts andris, since andris resolves the default sink when a playback
channel links. FreeRDP connects with `/sound:sys:fake`. The session's
`PULSE_SERVER` names pipewire-pulse's socket, so no PulseAudio client
can start a daemon of its own, and ryll gets an unreachable one.

Every clipboard check copies text not used before, because FreeRDP
and xrdp keep what they last fetched. Andris runs there with debug
logging for the clipboard, and both its log and the stand-in's carry a
hash of each text that passes, so a failed check shows which text went
where without the text itself.

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
repository and qemu-display, which only the optional `qemu` feature
uses; `andris/Cargo.toml` says why that is pinned to a commit. Add a
licence to the allowlist only when a dependency needs it, and give
every advisory ignore a URL and a rationale.
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
- **The release version check's fixtures**,
  [`tools/test-release-version.sh`](https://github.com/shakenfist/andris/blob/develop/tools/test-release-version.sh),
  which runs `tools/release-version.sh check` against fixture
  `Cargo.toml` and `Cargo.lock` files. It runs only when one of the
  two scripts changes.
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
| `Build and test` | `make build test`, then `make test FEATURES=qemu` |
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

Runs only on `merge_group` and gates `Can merge`. Four jobs run.
`Build and test the merge commit` runs `make release test`, then
`make test FEATURES=qemu`, against the commit the queue has built
from the pull request and the current tip of `develop`. The smoke tier's jobs do not run on `merge_group`.

The other three also run on `workflow_dispatch`, and `Can merge`
requires each of them. A retest of a branch dispatches them, so they
can be tried before the branch is enqueued; nothing gates on that run.

- `End to end` runs `make e2e`. It is in the merge tier because
  building ryll would add over a minute to every push; its cargo cache
  has a key prefix of its own, so that only a new ryll pin pays for
  the build, and so that the offline jobs never restore a cache the
  networked ryll build wrote.
- `Through kerbside` runs `make e2e-kerbside` (see
  [the kerbside lane](#the-kerbside-lane)), and uploads its redacted
  logs.
- `Build the wheel` runs `make wheel ARCH=x86_64` and then
  `scripts/wheel-smoke.sh` (see [the wheel](#the-wheel)), and uploads
  the wheel. Only the x86_64 wheel is built on a merge; the release
  workflow builds both.

### Gates and path filtering

Three jobs are the required status checks on `develop`:

| Gate | Runs on | Passes when |
|------|---------|-------------|
| `Can see status` | every event | always; it only proves status reporting works |
| `Can enqueue` | not `merge_group` | every smoke job succeeded or was skipped |
| `Can merge` | `merge_group` | all four merge-tier jobs succeeded or were skipped |

A `Check paths` job skips the jobs of both tiers when a pull
request, or a merge-queue entry, changes only `docs/**`, `LICENSE`
or the review state: `REVIEWS.md` and the weAudit marks under
`.vscode/` (see [Reviewing andris](#reviewing-andris)). Other
Markdown, such as `AGENTS.md`, is not skipped, since the `skillsaw`
hook lints it, and nor is `.vscode/review-scope.toml`, which decides
what is reviewed at all. The filter is applied per job and not with
a `paths-ignore:` on the trigger: a required check in a workflow
that never started never reports, and would block the merge forever.
The gates treat a skipped job as success for the same reason, and
since `Check paths` is itself a dependency of both gates, a failure
of the filter fails them.

### Other workflows

| Workflow | When | What it does |
|----------|------|--------------|
| `secret-scan.yml` | every pull request, pushes to `develop` | gitleaks over full history; not path-filtered, because a credential lands in documentation as easily as in code |
| `supply-chain.yml` | Mondays 09:00 UTC, and on demand | `cargo audit` and `cargo deny` against `develop`, so a newly published advisory is noticed without waiting for a pull request |
| `codeql-analysis.yml` | pull requests, pushes to `main` and `develop`, Tuesdays 17:00 UTC | CodeQL; skipped for changes to documentation and review state alone |
| `prune-reviews.yml` | pushes to `develop`, daily, and on demand | drops review marks whose file has changed since it was reviewed, imports marks for files identical to ones reviewed in shakenfist/development, and commits the result to `develop` as the bot; see [Reviewing andris](#reviewing-andris) |
| `renovate.yml` | hourly | self-hosted Renovate dependency updates |
| `release.yml` | a pushed `vX.Y.Z` tag; a manual dispatch builds and tests both wheels without publishing | checks that `ci.yml` passed on the tagged commit, builds the x86_64 and aarch64 wheels, signs the tag, creates the GitHub release and publishes to PyPI; see [releasing.md](releasing.md) |
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
   `merge_group`. `Build and test the merge commit`, `End to end`,
   `Through kerbside`, `Build the wheel` and `Can merge` run against
   it.
4. If they pass, the queue merges. If not, the pull request is
   ejected and the failure is in the `merge_group` run's log.

A failure on the merge tier is not necessarily the pull request's
fault: a flaky runner can cause one, so re-enqueue before assuming
otherwise. A semantic conflict with something merged to `develop`
since the smoke tier ran fails it too, and needs a rebase.

The queue's `ci.yml` run on the commit it merges is the CI result a
release requires: the release workflow refuses to publish a commit
that has none (see [releasing.md](releasing.md#the-ci-gate)). So a
change merged around the queue, by an administrator bypassing it,
cannot be released until a later commit has gone through the queue.

The queue builds one entry at a time (`max_entries_to_build: 1` in the
`Develop branch` ruleset). The merge tier's concurrency groups depend
on that: a queue run shares a group with whatever the queue builds
next, so that a superseded run is cancelled. Raising the setting would
make each entry cancel the one ahead of it, so revisit those groups
first.

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

## Reviewing andris

Andris is in the Shaken Fist ecosystem's review tracking, which
records which files a person has read and when. The mechanism is
described in
[development's code-review-tracking.md](https://github.com/shakenfist/development/blob/main/docs/code-review-tracking.md).
The files in scope are listed in `.vscode/review-scope.toml`, and
`REVIEWS.md` is generated from the review state, never edited by hand.

`./tools/review-tracking.sh next` offers the next file to read in the
order of [the reading guide](reading-guide.md), which says what each
file is for and what to look at. Review sessions land as pull requests
from a `reviews` branch. The `review-coverage` audit keeps an issue
open while five or more in-scope files need review.
