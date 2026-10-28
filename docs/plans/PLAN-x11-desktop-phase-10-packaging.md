# Phase 10: Packaging and dogfood soak

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: medium, as the master plan asks. Packaging follows
ryll's wheel and release tooling closely. Two things lift parts of it
above the pattern. First, phase 9 made andris link libpulse, which the
master plan's packaging premise did not foresee (finding 1). Second,
the kerbside lane composes three projects' test stacks.

## Scope

In scope, as one andris pull request, then a release, a soak, and an
acceptance pull request (decision 9):

- **libpulse loaded at run time** rather than linked (decision 2), so
  that the wheel earns its `manylinux_2_28` tag. Without libpulse,
  andris still starts and serves the desktop without sound.
- **Wheels on PyPI.** Two `manylinux_2_28` wheels, x86_64 and
  aarch64, built with maturin's `bindings = "bin"` as ryll's are,
  each with openh264 compiled in (decision 1). A merge-tier CI job
  builds the x86_64 wheel and smoke-tests it in a slim container. A
  tag-triggered release workflow builds both wheels, signs the tag,
  and publishes through PyPI trusted publishing.
- **`andris install-unit`** (decision 5). It writes the systemd user
  unit with the running binary's absolute path, and creates the
  configuration directory and a ticket when they are missing.
- **`SystemCallFilter=@system-service`** in the unit. Phase 6 left it
  for the soak to test (decision 6).
- **The kerbside lane** (decision 7): `make e2e-kerbside`, a
  merge-tier CI job. It runs andris's end-to-end checks through
  kerbside's demo stack, so that every channel, the playback channel
  among them, crosses kerbside's allowlist on every merge.
- **Documentation**: installing from PyPI, releasing, the kerbside
  lane, and a recipe for running kerbside's demo stack in front of a
  live session.
- **The first release, 0.1.0**, and **a week's dogfood soak** of
  ryll → kerbside → andris on the development host, with RDP as the
  fallback. Regressions are filed and either fixed or recorded under
  Future work.

Out of scope:

- **A `.deb` or `.rpm`.** The master plan calls a `.deb` optional,
  and the wheel plus `install-unit` covers the install story. This
  goes to Future work.
- **Publishing andris to crates.io, and moving off ryll's git
  revision** (decision 4).
- **musllinux wheels, macOS and Windows.** The operator decided in
  open question 6 that andris is Linux only, and it targets glibc
  desktops.
- **Running systemctl from andris.** `install-unit` writes files and
  prints the commands to run; it does not enable anything (decision
  5).
- **Fixing what the soak finds, beyond regressions.** Ideas for
  improvement are filed and go to Future work. Only regressions that
  block daily use are fixed in this phase.

## What the survey found

The survey read andris at `develop` `d8f316a` (phase 9's merge), and
also:

- ryll at `629b1d8`, with tag `v0.3.0` at `f12d9b2`;
- kerbside at `1780e3c`, with tag `v0.7.0` at `2d9ee91`;
- development at `cc5216e`;
- the installed `/usr/local/bin/andris`, inspected only with
  `readelf`.

Phase 9's completion check is under *Phase 9's completion check*,
below.

1. **Andris no longer links only manylinux libraries.** The master
   plan says andris's X11 code is pure-Rust `x11rb`, "so the
   manylinux tag holds without vendoring anything". The X11 half is
   true. Since phase 9, though, `readelf -d /usr/local/bin/andris`
   lists `libpulse-simple.so.0` and `libpulse.so.0` as `NEEDED`,
   through `libpulse-binding` and `libpulse-simple-binding`
   (`andris/Cargo.toml`). Neither is on the `manylinux_2_28`
   allowlist. The other entries are allowed:
   - `libstdc++.so.6` (openh264 is C++);
   - `libgcc_s.so.1`, `libm.so.6`, `libc.so.6` and the dynamic
     linker.

   Opus is built statically from bundled source, through opusic-sys.
   So libpulse is the only obstacle, and decision 2 removes it.
2. **Ryll has the same problem, and documents a compromise.** Ryll's
   `tools/build-ryll-wheel-in-container.sh` builds with
   `--skip-auditwheel`. It asserts the tag anyway, with one allowed
   exception, `libasound.so.2`, and a `DT_NEEDED` check fails the
   build on any other. Ryll's `docs/releasing.md` ("The PyPI wheel's
   platform tag") calls `dlopen` "the real fix". Ryll could not take
   it, because `alsa-sys` has no such mode. Andris's use of libpulse
   is five functions of the simple API, so it can take the real fix.
3. **Building openh264 without nasm silently loses its SIMD.**
   `.devcontainer/Dockerfile:9` records that without nasm, openh264
   "silently skips its SIMD code, and a 1080p encode takes about
   2.7x as long". Ryll's wheel image installs no nasm. Andris's
   must, and the build must check that the SIMD code went in.
4. **The kerbside documentation is already written.** The master
   plan asks this phase to "write the kerbside static-source
   configuration into `docs/`". `docs/running.md`'s *Fronting andris
   with kerbside* has done so since phase 4. What is missing is a
   recipe for putting kerbside in front of a live session, which the
   soak needs.
5. **The kerbside check has never covered playback or streams.**
   Phase 4's manual smoke test, recorded in that plan's Result
   section, ran before video and audio existed. Nothing has sent
   `STREAM_*` or any playback message through kerbside. Ryll's
   `message_names` names them all, so kerbside should accept them,
   but nothing has shown that it does.
6. **Kerbside's demo stack is the stack the lane needs.** Kerbside
   v0.7.0 ships `demo/`, a compose stack with MariaDB, a released
   kerbside from PyPI and a static source. Its `spice-target`
   service is a qemu that the lane can replace. Kerbside's own
   `demo-compose.yml` lane drives it with `tools/demo/lane-up.sh` and
   `lane-assert.sh`. Two things need working around:
   - Andris refuses a non-loopback listener without TLS, and the
     demo reaches its target over the compose network.
   - Phase 4's smoke test solved this by running andris in kerbside's
     network namespace, on `127.0.0.1:5910`.
7. **Ryll 0.3.0 predates the pin.** Andris pins ryll at `38c51ef`,
   ryll#578's merge, which `v0.3.0` does not contain:
   `git merge-base --is-ancestor v0.3.0 38c51ef` holds, and
   `v0.3.0..38c51ef` is #578's three commits. Moving to crates.io
   versions would therefore need a ryll release, and nothing in
   phase 10 needs it (decision 4).
8. **`docs/running.md` still points at ryll 0.1.7.** *Connecting
   with ryll* says to use a ryll from `develop` "until a ryll
   release after 0.1.7 carries the same changes". Ryll 0.3.0 now
   does, for everything but the `stats` section that only `make e2e`
   uses. Step 6 corrects this.
9. **The unit names a bare `andris`.** As the master plan says,
   `packaging/systemd/andris@.service` has `ExecStart=andris ...`,
   which systemd resolves only in its own binary path. The CLI is a
   flat clap struct with no subcommands (`andris/src/config.rs:49`),
   so `install-unit` is andris's first subcommand.
10. **The release audit has rules.** Development's
    `scripts/audit/checks/packaging.py` checks release workflows. Its
    rules cover:
    - release assets and the release workspace;
    - container paths;
    - agreement on `dist/`;
    - guards on a release workflow's `workflow_dispatch`.

    `ci_workflows.py` requires `audit-ok: github-hosted-runner` on
    GitHub-hosted runners. The release workflow must pass these, so
    step 4 runs the audit.
11. **The name is free on PyPI.** `https://pypi.org/pypi/andris/json`
    returned 404 on 2026-10-18.

The master plan's false or stale claims are corrected at their
source in this phase's planning commit:
- the phase 10 section (findings 1 and 4);
- the phase 3 note on crates.io (finding 7);
- open question 7, now answered;
- the `index.md` row.

Later steps need not redo this.

### Phase 9's completion check

- **Status.** The master plan's Execution table and `index.md`
  agreed (`In progress`) before this phase's close-out commit.
- **On `develop`.** Andris#27's merge `d8f316a` is on `develop`, and
  ryll#578's merge `38c51ef` is ryll's pin.
- **Definition of done, spot-checked:**
  - `PLAYBACK_CAP_OPUS` is in ryll's protocol crate;
  - the pin appears three times in `andris/Cargo.toml`;
  - `grep -i "no audio"` over the four named documents finds
    nothing;
  - the 10 ms test bounds every packet at 480 bytes.
- **Nothing left over.** No worktree or branch remains. A local
  tracking ref for `origin/x11-desktop-phase-09-audio` remains, but
  the branch is gone from GitHub.
- **Deferred work is recorded.** Future work has volume and mute,
  following the default sink, and `AUDIO_VOLUME_SYNC`. Ryll has
  #575 and #576.

## Decisions

1. **The wheels ship openh264 compiled from source.** *Decided by the
   operator on 2026-10-18*, answering open question 7. This is what
   ryll's wheels already do. The Cisco-binary route would add an
   operator download, plus a version lock-step on every openh264
   bump, for a patent position that `docs/running.md` already states.
   The *Patents* paragraph drops "undecided" and says what the
   wheels carry.

2. **libpulse is loaded at run time, not linked.** Andris loads
   `libpulse-simple.so.0` with `libloading`, through hand-written
   bindings to the five functions it uses: `pa_simple_new`,
   `pa_simple_read`, `pa_simple_get_latency`, `pa_simple_free` and
   `pa_strerror`. It does this once, the first time it is needed.
   Then:
   - the wheel links nothing outside the `manylinux_2_28` policy, so
     the tag is earned, not asserted;
   - maturin's own auditwheel check, run without
     `--skip-auditwheel`, becomes the guard, so there is no
     exception list to maintain;
   - an andris with no libpulse installed still starts. With
     `--platform x11`, it logs one warning naming the package to
     install, and serves as `--no-audio` does: no playback channel.

   This is the decision a reviewer is most likely to dispute. Ryll's
   answer to the same problem was a one-line exception plus a
   documented runtime requirement, and that would be cheaper here
   too. Two things decide it the other way:
   - **The library is optional for andris's uses.** The synthetic
     platform and `--no-audio` need no libpulse. The synthetic
     platform is meant for kerbside load testing, often in slim
     containers. A hard link makes all of these fail at exec with
     `error while loading shared libraries`, before andris can say
     anything.
   - **The surface is tiny.** Ryll could not take this route; andris
     can, with about a hundred lines.

   The cost is a second module that may use `unsafe`. That changes
   the `AGENTS.md` convention, which today allows `unsafe` only in
   `platform/x11/shm.rs`. Keeping `libpulse-sys` as a
   *dev*-dependency lets a test check the hand-written constants and
   struct layouts against the real headers' bindings, so the
   bindings cannot drift unseen.

3. **Wheels for x86_64 and aarch64, and nothing else.** Both are
   `manylinux_2_28`, built natively, as ryll's are. The x86_64 wheel
   is built on the self-hosted `debian-13-docker` runner, and the
   aarch64 wheel on GitHub's `ubuntu-24.04-arm`. There is no sdist:
   maturin's `bindings = "bin"` wheel is the product, and an sdist
   would need ryll's git crates at build time. The aarch64 leg is
   cheap because ryll already builds openh264 there, and andris is
   tested only on x86_64.

4. **Ryll stays a git dependency.** A wheel embeds a compiled binary,
   so a git dependency costs it nothing. Only a crates.io release of
   andris would forbid one, and andris is a binary that nobody
   depends on. The master plan's phase 3 note ("phase 10 is the
   latest point at which that matters") is corrected to say so.
   Moving to crates.io versions goes to Future work, to happen once a
   ryll release contains #578.

5. **`andris install-unit` writes, and prints; it never runs
   systemctl.** It works as follows:
   - **The unit.** It writes the embedded
     `packaging/systemd/andris@.service`, with `ExecStart=` naming
     the running binary's absolute path (`std::env::current_exe`), to
     `$XDG_CONFIG_HOME/systemd/user/`, or `~/.config/systemd/user/`
     when that is unset.
   - **Overwriting.** It always overwrites, and says whether the
     unit was written, replaced or unchanged. Local changes belong in
     a drop-in (`systemctl --user edit andris@`), which the docs say.
     Running it again after moving the wheel is how `ExecStart`
     follows the binary.
   - **Configuration.** It creates `~/.config/andris/` with mode 0700
     if missing. It writes a fresh 0600 ticket only if none exists,
     using `create_new` so that a race cannot clobber one.
   - **Next steps.** It then prints `systemctl --user daemon-reload`
     and the `enable --now` line. The display number comes from
     `$DISPLAY` when that names a local display, and is otherwise a
     placeholder with a pointer to `docs/xrdp.md`. It only reads
     `$DISPLAY`; it never connects to it. *(Step 7 replaced the
     `enable --now` line with `enable` followed by `restart`: `--now`
     does nothing to an instance that is already running, which left
     the old binary serving after a reinstall. See the step 7 Result.)*

   Running systemctl itself would make the command's tests touch a
   real user manager, and would make enabling a side effect of
   installing. The `ExecStart` path is double-quoted, with `%`
   doubled. Step 2 checked the remaining rules against systemd 257's
   `systemd-analyze verify`, which corrected this decision's first
   draft:
   - `$` is left as it is, because systemd does not expand variables
     in the executable path;
   - a `"`, `'` or `\` is refused, because systemd rejects them in an
     executable path whether or not they are escaped;
   - a control character, a path that is not UTF-8 and a relative
     path are refused too.

   The unit names its ticket and environment file through `%E`,
   which a user manager defines as `$XDG_CONFIG_HOME`, so the unit
   and `install-unit` agree on where they are.

6. **The unit gains `SystemCallFilter=@system-service`, killing on a
   violation.** systemd's default action kills the process with
   SIGSYS. The journal then says `status=31/SYS`, which is easy to
   find. An `EPERM` would instead surface as some odd failure deep
   in MIT-SHM or libpulse. `NoNewPrivileges=yes` is already set,
   which a user unit's filter needs. The soak is the test that phase
   6 asked for: zero `status=31/SYS` lines over the week, with
   MIT-SHM capture and sound in use. If it fails, the acceptance pull
   request either adds the missing group or removes the filter, and
   says which.

7. **The kerbside lane runs in andris's merge tier, behind `Can
   merge`.**
   - **The stack.** `make e2e-kerbside` checks out kerbside's
     `demo/` at a pinned commit, v0.7.0's `2d9ee91`. The demo's
     default installs the *released* kerbside from PyPI, which is the
     kerbside an operator runs.
   - **The compose override.** A compose override, kept in andris,
     replaces `spice-target` with a placeholder. It points
     `sources.yaml` at `127.0.0.1:5910` with a ticket the lane
     generates.
   - **Andris and ryll.** Andris, ryll headless, Xvfb, PulseAudio and
     the driver all run in one `andris-dev` container, joined to
     kerbside's network namespace (`--network container:...`). This
     keeps andris on loopback, as phase 4's manual smoke test did.
     Ryll reaches kerbside's TLS port on the same loopback, with a
     `.vv` from the demo's `get-console.sh`.
   - **Network.** The run uses an internal compose network after the
     images are built, so that andris's compiled code gets no more
     network than `make e2e` gives it.
   - **What it checks.**
     - The driver runs the subset of `e2e-drive.py` that crosses
       every channel: a frame, keys, clipboard both ways, one H.264
       stream, and one Opus stream.
     - Kerbside's log must show "hypervisor connection successful"
       for every channel, the playback channel among them, and no
       firewall verdict or termination.

   Putting it behind `Can merge` makes it required without a ruleset
   change, as `End to end` is. The kerbside pin is bumped by hand,
   which `docs/development.md` says, because Renovate does not track
   a commit in a script.

8. **The wheel is built and smoke-tested on every merge, for
   x86_64.** A merge-tier job, `Build the wheel`, also gated by `Can
   merge`, runs `make wheel` and then installs the wheel in a
   `python:3-slim` container. That image deliberately has no
   libpulse. In it, the job checks that:
   - `andris --version` prints the version;
   - `andris install-unit` writes a unit into a scratch
     `XDG_CONFIG_HOME`, whose `ExecStart` names the installed binary;
   - `andris --platform synthetic` starts, logs that it is listening,
     and exits 0 on SIGTERM;
   - `--platform x11` without libpulse logs the warning from decision
     2, rather than failing at exec. It needs no X server for this:
     the check is on the log line before the display is opened, or
     the step says why that is impossible.

   The aarch64 wheel is built only by the release workflow, and by a
   guarded `workflow_dispatch` on it that builds without publishing,
   if development's dispatch-guard rule allows one.

9. **Merge, then release, then soak; then a separate acceptance pull
   request.** The packaging pull request lands first, after CI, the
   reviewer and a short operator check of the wheel on `:10` (step
   7). Then `make propose-release 0.1.0` opens a release pull
   request, and the operator tags the merged commit, which builds,
   signs and publishes. The soak runs on what PyPI serves, so it
   tests the released artifact rather than a branch build. Its
   results, and any fixes, land as an acceptance pull request, as
   phase 8's did (andris#23). Holding the packaging pull request open
   for a week would only invite conflicts, and soaking a branch build
   would leave the release untested.

10. **The soak goes through kerbside's demo stack on the development
    host.** *Decided by the operator on 2026-10-18.* Kerbside cannot
    share andris's network namespace there, because andris runs on
    the host under systemd. So the recipe, written in step 6 and
    tried in step 11, works like this:
    - Kerbside's container uses host networking, with its SPICE,
      redirect and API listeners bound to `127.0.0.1`. The kerbside
      bind option is `kerbside/config.py:166`; step 6 confirms that
      it covers all three listeners.
    - Andris listens on `127.0.0.1:5910` through `ANDRIS_ARGS`.
    - The Mac reaches kerbside through an SSH tunnel of 5900, 5901
      and 13002, so the `.vv`'s `127.0.0.1` and the proxy
      certificate's SAN both hold.
    - Nothing listens beyond loopback.
    - If kerbside cannot bind all three listeners to loopback, the
      fallback is andris on TLS at the Docker bridge address, with
      kerbside's backend TLS (`secure_port`, `ca_cert`,
      `host_subject`). Step 6 records which was used.

11. **The release tooling is ryll's, adapted, with andris's
    differences.** It copies these from ryll and adapts them:
    - `scripts/propose-release.sh`, `scripts/tag-release.sh` and
      `scripts/sign-release-tag.sh`;
    - the `release` environment, with the operator's approval
      gating signing and publishing;
    - Sigstore signing of the tag;
    - publishing with `pypa/gh-action-pypi-publish` and build
      attestations.

    Andris differs in three ways:
    - **No crates or packages.** There is no crates.io job, and no
      `.deb`, `.rpm`, macOS or Windows leg.
    - **The GitHub release** carries the two wheels and their
      `SHA256SUMS`.
    - **Script location.** Andris keeps scripts in `scripts/`, not
      `tools/`. Copying from ryll is within one organisation and is
      not the translation the master plan forbids.

## Execution

Implementation is done by sub-agents briefed from this plan. The
management session reviews each diff and makes each commit.

- Every commit must leave `make lint` and `make test` passing, and
  `pre-commit run --all-files` clean.
- Cargo runs only through the `Makefile` or the wheel scripts, in
  Docker, never on the host.
- No code is translated from spice-server, spice-gtk, x11spice, QEMU,
  xrdp, gnome-remote-desktop or the openh264-rs examples. Ryll's and
  kerbside's own scripts may be adapted.
- Andris must not depend on `shakenfist-spice-renderer`.
- **No sub-agent connects to, signals, captures or injects into the
  host's `:10` session**, nor touches its xrdp, sesman or chansrv.
  The host shell has `DISPLAY=:10.0`. Run every make target and X
  command as `env -u DISPLAY -u XAUTHORITY -u PULSE_SERVER -u
  PULSE_COOKIE make ...`.
- **No sub-agent connects to the host's sound server.** Never mount
  `$XDG_RUNTIME_DIR` or `/run/user` into a container.
- **No sub-agent writes to the real `~/.config`**, runs `systemctl
  --user`, or installs anything into the host's Python. Tests of
  `install-unit` take their directories as arguments, and smoke tests
  run in containers.
- **No sub-agent pushes a tag, publishes to PyPI, or changes GitHub
  or PyPI settings.** Those are steps 9 and 10, with the operator.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Complete | Commit phase 9's close-out and this plan. See brief 0. |
| 1 | high | opus | none | Complete | Load libpulse at run time; serve without sound when it is missing. See brief 1. |
| 2 | medium | opus | none | Complete | `andris install-unit`, and `SystemCallFilter` in the unit. See brief 2. |
| 3 | high | opus | none | Complete | The wheel: pyproject, build image, build scripts, `make wheel`, the earned-tag and SIMD checks, the merge-tier job and its slim-container smoke test. See brief 3. |
| 4 | medium | opus | none | Complete | The release workflow and scripts, and `docs/releasing.md`; the consistency audit passes. See brief 4. |
| 5 | high | opus | none | Complete | The kerbside lane: `make e2e-kerbside` and its merge-tier job. See brief 5. |
| 6 | medium | sonnet | none | Complete | Documentation, including the soak recipe. See brief 6. |
| 7 | medium | management | none | Complete | Operator check of the wheel on `:10`. See brief 7. |
| 8 | medium | management | none | Complete | Push audit and pull request. See brief 8. |
| 9 | low | operator | none | Complete | PyPI trusted publisher, the `release` environment, tag protection. See brief 9. |
| 10 | low | management | none | Complete | Release 0.1.0. See brief 10. |
| 11 | medium | operator | none | Complete | The week's soak through kerbside. See brief 11. |
| 12 | medium | management | none | Complete | The acceptance pull request. See brief 12. |

Steps 1 and 2 are independent, and either may go first. Step 3 needs
step 1, since its earned-tag check fails until libpulse is no longer
linked. Step 4 needs step 3, and step 5 may run beside steps 3 and 4.
Step 6 follows steps 1 to 5, then step 7, then step 8. Step 9 may
happen at any time before step 10, and step 10 needs step 8's pull
request merged. Steps 11 and 12 follow in order.

### Brief 0: commits

Two commits, in order:

1. Phase 9's close-out: the master plan's Execution row (`Complete`,
   with `ryll 38c51ef (#578); andris d8f316a (#27)` under `Merged`)
   and the `index.md` row.
2. This plan, registered in the Execution table and `index.md`
   (`In progress`), with the master plan's corrections from the
   survey.

### Brief 1: libpulse at run time

Read first:
- `andris/src/platform/pulse.rs`, the whole module and its tests;
- `andris/src/lib.rs`'s `audio_source`;
- `andris/tests/pulse.rs`;
- `AGENTS.md`'s `unsafe` rule;
- `andris/src/platform/x11/shm.rs`, for how an `unsafe` module is
  written here, with `SAFETY:` comments.

Then:

1. Replace `libpulse-binding` and `libpulse-simple-binding` with
   `libloading` (0.8, ISC, which `deny.toml` allows). Add
   `libpulse-sys` as a dev-dependency only, at the version
   `libpulse-binding` pulled in.
2. Make `platform/pulse.rs` a directory module, `platform/pulse/`,
   with a new `ffi.rs` holding the only `unsafe`. Declare
   `#![allow(unsafe_code)]` in that file alone. It holds:
   - `#[repr(C)]` mirrors of `pa_sample_spec` and `pa_buffer_attr`;
   - the constants used: `PA_SAMPLE_S16LE`, `PA_STREAM_RECORD`, and
     `PA_OK` if needed;
   - a function-pointer table for the five functions in decision 2;
   - a safe wrapper type that owns a `pa_simple*` and frees it on
     drop.

   **Take every value from the headers in the devcontainer**
   (`/usr/include/pulse/def.h`, `sample.h`, `simple.h`), not from
   memory, and cite the header in a comment.
3. Load `libpulse-simple.so.0` (its own `NEEDED` brings in
   `libpulse.so.0`) once, into a process-lifetime static
   (`OnceLock`), and never unload it: capture threads are never
   joined (see the module doc's *Stopping*), so one may still be
   inside a read. Expose the result as `pulse::load() -> Result<(),
   LoadError>`.
4. In `lib.rs`'s `audio_source`, for `AudioConfig::Pulse`, call
   `load()` first. On failure, log one `warn!` and return `None`, as
   `AudioConfig::Off` does. The warning must say:
   - that libpulse could not be loaded, with the loader's error;
   - that andris serves without sound;
   - to install `libpulse0` (Debian, Ubuntu) or `pulseaudio-libs`
     (Fedora, RHEL).

   Nothing else changes: the playback channel is simply not offered,
   as with `--no-audio`. Make the library name injectable for a
   test, as a parameter of an internal function, not an environment
   variable.
5. Keep the capture thread's behaviour, logging, `MAX_LIVE_CAPTURES`
   and latency fallback exactly as they are. Only the calls change.
6. Tests:
   - a unit test asserting each hand-written constant, and
     `size_of`/`align_of` of each mirrored struct, against
     `libpulse-sys`;
   - a test that a library name that does not exist yields
     `audio_source` → `None` with the warning;
   - `andris/tests/pulse.rs` must pass unchanged under `make e2e`,
     which proves the bindings against a real daemon.
7. Update `AGENTS.md`'s `unsafe` rule to name both modules, and the
   comment on the libpulse dependencies in `andris/Cargo.toml`.
   Leave `libpulse-dev` in `.devcontainer/Dockerfile`, since the
   dev-dependency links it. Its comment should now say that tests
   need it and the binary does not.
8. Verify with `make lint test e2e`. Check that `readelf -d
   target/release/andris` (after `make release`) lists no libpulse.
   Mutate the tree: link `libpulse-simple-binding` back in
   temporarily, and confirm the `readelf` check would catch it. Step
   3 makes that check permanent.

Commit subject: `Load libpulse at run time.`

### Brief 2: `andris install-unit`

Read `andris/src/config.rs`: the clap `Args` struct, `open_private_file`
and `read_ticket_file`. Also read `andris/src/main.rs`,
`packaging/systemd/andris@.service`, and `docs/xrdp.md`'s
*Installing* and *Configuring* sections, which show the files this
command creates.

1. Add a clap subcommand, `install-unit`, alongside the existing flat
   options. Use `Option<Command>` with `args_conflicts_with_subcommands`,
   so that `andris --display :10 ...` keeps working unchanged. Running
   `andris install-unit` must not read a ticket, open a display or
   start a runtime. Give it no options beyond `--help`.
2. Embed the unit with `include_str!` from
   `packaging/systemd/andris@.service`, so the file stays the one
   source.
3. Write a pure function, `render_unit(template: &str, exe: &Path) ->
   Result<String>`. It replaces the bare `andris` at the start of the
   one `ExecStart=` line with the escaped absolute path (decision 5's
   rules), and fails if there is not exactly one such line. Unit
   tests cover:
   - a plain path;
   - a path with a space, `%`, `$`, `"` and `\`;
   - a newline (refused);
   - a template with zero or two `ExecStart=andris` lines (refused).

   Check the escaping rules against `systemd.service(5)` and
   `systemd.syntax(7)`, as shipped in the devcontainer if the man
   pages are present, or else against systemd's documentation. Do not
   rely on memory.
4. Write a second function, `install(config_home: &Path, exe: &Path)
   -> Result<Report>`. It:
   - writes `systemd/user/andris@.service` under `config_home`,
     creating directories as needed, and reports Written, Replaced or
     Unchanged;
   - creates `andris/` mode 0700 when missing, and leaves an
     existing one alone. If an existing one is group- or
     world-accessible, it says so, which `open_private_file` would
     refuse anyway;
   - writes `andris/ticket` with mode 0600, using
     `OpenOptions::create_new` and 32 characters of base64 from 24
     random bytes, as `docs/xrdp.md` makes one, but only when no
     ticket exists.

   `main` resolves `config_home` from `XDG_CONFIG_HOME`, else
   `$HOME/.config`, and resolves `exe` from `current_exe()`. Then it
   prints the report and the next commands (decision 5). Tests use
   `tempfile` directories only. **No test may touch the real home
   directory or run systemctl.**
5. Add `SystemCallFilter=@system-service` to the unit, after the
   other hardening lines. Its comment must say:
   - that a violation kills andris with SIGSYS, which the journal
     shows as `status=31/SYS`;
   - that it was adopted in phase 10, pending the soak.

   Run `systemd-analyze verify` on the rendered unit inside a
   container, if the devcontainer has systemd's tools. If it does
   not, say so.
6. Verify with `make lint test`.

Commit subject: `Add andris install-unit.`

### Brief 3: the wheel

Read ryll's packaging first:
- `.devcontainer/wheel/Dockerfile`;
- `tools/build-ryll-wheel.sh` and `tools/build-ryll-wheel-in-container.sh`;
- `ryll/pyproject.toml`;
- `docs/releasing.md`'s *The PyPI wheel's platform tag*.

Then read andris's `Makefile`, especially its network-isolation
comments, its `.devcontainer/Dockerfile`, and `.github/workflows/ci.yml`.

1. Write `andris/pyproject.toml`, modelled on ryll's. Use `name =
   "andris"`, `bindings = "bin"`, and a dynamic version from the
   workspace. Classifiers say Linux only. Its header comment says the
   tag is earned, because libpulse is loaded at run time (decision 2).
2. Write `.devcontainer/wheel/Dockerfile` on
   `quay.io/pypa/manylinux_2_28_${ARCH}`. It installs:
   - gcc, g++, make, cmake and pkgconf;
   - **nasm** (EL8's PowerTools or EPEL; find which);
   - a Rust toolchain matching the version the andris devcontainer
     installs;
   - maturin `>=1.7,<2`.

   Nothing else, since andris links no system library.
3. Write `scripts/build-andris-wheel.sh <x86_64|aarch64>` and
   `scripts/build-andris-wheel-in-container.sh`, keeping ryll's split:
   - `cargo fetch --locked` runs with the network up;
   - the maturin build runs with `--network none` and a read-only
     cache.

   Then:
   - Build with `maturin build --release --frozen --compatibility
     manylinux_2_28` and **without `--skip-auditwheel`**, so maturin
     itself refuses an out-of-policy library.
   - Keep a `DT_NEEDED` listing in the build log, for the record.
   - Add a SIMD check: the binary carries openh264's assembly
     (finding 3). On x86_64, look for a symbol from its x86 assembly,
     such as one ending `_sse2` or `_avx2`, using `nm` on the
     unstripped target binary before maturin strips it. On aarch64,
     look for a `_neon` symbol. Fail if none is found. Find a symbol
     that is actually present rather than guessing names, and say
     which in a comment.
4. Add `make wheel ARCH=x86_64`, defaulting to the host's
   architecture.
5. Add a merge-tier job to `ci.yml`, `Build the wheel`. It runs on
   `merge_group` and `workflow_dispatch` like `End to end`, on
   `[self-hosted, vm, debian-13-docker, l]`, and `Can merge` requires
   it. After `make wheel` it runs `scripts/wheel-smoke.sh`, which
   performs decision 8's checks in `python:3-slim`. Pin that image by
   digest, which Renovate can bump. Upload the wheel as an artifact.
6. Mutate the tree, keeping the mutations in a script under the
   scratchpad and reporting the count:
   - link `libpulse-simple-binding` back in, and confirm `make wheel`
     fails, naming the library;
   - remove nasm from the image, and confirm the SIMD check fails;
   - break `ExecStart` substitution in a copy of the template, and
     confirm the smoke test's `install-unit` check fails.
7. Add `target/wheels` handling to `make clean` if it is not already
   covered.

Commit subject: `Build andris as a manylinux wheel.`

### Brief 4: the release workflow

Read ryll's release tooling:
- `.github/workflows/release.yml`;
- `tools/propose-release.sh`, `tools/tag-release.sh` and
  `tools/sign-release-tag.sh`;
- the `propose-release` and `tag-release` targets in its `Makefile`;
- `docs/releasing.md`.

Then read development's `scripts/audit/checks/packaging.py`, in
particular `release_*_issues` and `release_dispatch_guard_issues`,
and `ci_workflows.py`.

1. Write `.github/workflows/release.yml`, triggered by `v*` tags.
   - **Jobs:**
     - `check-version`: the tag must equal `[workspace.package]`
       version;
     - `build-wheels`: x86_64 on `[self-hosted, vm,
       debian-13-docker, l]`, aarch64 on `ubuntu-24.04-arm` with its
       `audit-ok` comment, each running
       `scripts/build-andris-wheel.sh`;
     - `sign-tag`: environment `release`, as ryll's;
     - `github-release`: the wheels and a `SHA256SUMS`;
     - `publish-pypi`: environment `release`, `id-token: write`,
       attestations, `pypa/gh-action-pypi-publish@release/v1`.
   - **Permissions:** least privilege per job, with `permissions: {}`
     at the top.
   - **Dispatch:** add a guarded `workflow_dispatch` that builds
     both wheels and publishes nothing, if the audit's dispatch-guard
     rule permits. Otherwise leave dispatch out and say why in a
     comment.
2. Adapt the three scripts into `scripts/`, with `make
   propose-release X.Y.Z` and `make tag-release X.Y.Z`. Andris has
   one version, in the root `Cargo.toml`'s `[workspace.package]`,
   plus `Cargo.lock`'s andris entry. `propose-release` bumps both
   through `make lock`, not a hand edit, branches as `release-X.Y.Z`
   and pushes. It does not open a pull request; the management
   session does that in step 10.
3. Write `docs/releasing.md`, with sections on:
   - prerequisites: the PyPI trusted publisher, the `release`
     environment and tag protection, exactly as step 9 sets them up;
   - the two-stage process;
   - the artifacts;
   - the platform tag, briefly: earned, and why (decision 2);
   - troubleshooting.

   Link it from `docs/index.md` and README's documentation list.
4. Run development's audit against the worktree
   (`scripts/audit-check.py`, from a development checkout; read its
   usage). Fix every failure it reports for andris. Record the
   result in the commit message.
5. Lint the workflow with whatever pre-commit already runs on
   workflows, actionlint if present.

Commit subject: `Add a release workflow for PyPI.`

### Brief 5: the kerbside lane

Read the following:
- in andris: `scripts/e2e.sh` and `scripts/e2e-drive.py`, the
  Makefile's `e2e` and `ryll-headless` targets with their isolation
  comments, and `docs/development.md`'s *The end-to-end test*;
- in kerbside at `v0.7.0` (`2d9ee91`): `demo/docker-compose.yml`,
  `demo/sources.yaml`, `demo/get-console.sh`, `demo/README.md`,
  `tools/demo/lane-up.sh`, `tools/demo/lane-assert.sh`,
  `tools/demo/redact-artifacts.sh` and
  `.github/workflows/demo-compose.yml`;
- phase 4's plan, the step 9 smoke result in its Result section, for
  what kerbside's logs show.

1. Add `make e2e-kerbside`, which depends on `build` and
   `ryll-headless`. It:
   - fetches kerbside at the pinned commit into a per-commit
     directory under the cargo cache, as `ryll-headless` stages ryll.
     Pin the commit in one variable, with a comment on how to bump
     it;
   - runs `scripts/e2e-kerbside.sh`.
2. `scripts/e2e-kerbside.sh` does the following:
   - **The override.** Bring the demo up with a compose override
     from `scripts/kerbside-lane/`. It replaces `spice-target` with
     a placeholder that sleeps, and mounts a `sources.yaml` naming
     `127.0.0.1:5910` and a ticket generated per run. The ticket is
     written 0600 under the run's temporary directory, never
     committed.
   - **Bring-up.** Build the images with the network up. Then bring
     the stack up on a compose network marked `internal: true`, with
     the published ports reset (`!reset`), since nothing outside the
     namespace needs them. If the stack cannot start that way,
     because kerbside needs egress at run time, record what needed
     it in the script's comments and the commit message, and stop
     for the management session rather than opening the network.
   - **Readiness.** Wait for kerbside's health check, then mint a
     `.vv` with the demo's `get-console.sh`, running inside the
     kerbside container if that is how it works.
   - **The test container.** Run the `andris-dev` image with
     `--network container:<kerbside container>`. Mount the checkout
     read-only and target/ writable, as the `e2e` target does. In
     it, run `scripts/e2e.sh` in a new kerbside mode (`E2E_VIA=kerbside`,
     `E2E_VV=<path>`, `PORT=5910`). In that mode e2e.sh:
     - starts andris on `127.0.0.1:5910` with the lane's ticket file;
     - hands ryll the minted `.vv` instead of writing its own;
     - runs the driver's frame, keys, clipboard, H.264 stream and
       tone checks, and checks that ryll logged all five channels;
     - skips the second ryll, the `--no-audio` andris and the Xvfb
       kill, which test andris rather than kerbside.

     If kerbside's tokens are single use, mint one per ryll, and say
     so in a comment.
   - **Kerbside's log.** Assert that it shows "hypervisor connection
     successful" for main, display, cursor, inputs and playback, and
     no firewall verdict or termination. Take the exact strings from
     `lane-assert.sh` and phase 4's result, not from memory.
   - **Teardown.** On exit, always collect compose logs, redact the
     `.vv` token and the MariaDB root password (follow
     `redact-artifacts.sh`), and run `docker compose down -v`.
3. Add a merge-tier job, `Through kerbside`, to `ci.yml`. It runs on
   `merge_group` and `workflow_dispatch`, on `[self-hosted, vm,
   debian-13-docker, l]`, with its own cargo-cache key prefix as `End
   to end` has, and `Can merge` requires it. Set `no_proxy` for
   loopback if the runner's proxy needs it, as kerbside's lane
   records. Upload the redacted logs.
4. Mutate the tree:
   - add a message type that ryll's `message_names` lacks to a
     scratch build, or point the lane at a kerbside allowlist that
     drops the playback channel, and confirm the lane fails naming
     kerbside's verdict;
   - break the ticket, and confirm it fails at the link.

   Report what was run.
5. Run it locally with `env -u DISPLAY -u XAUTHORITY -u PULSE_SERVER
   -u PULSE_COOKIE make e2e-kerbside`, and report its wall time.

Commit subject: `Test andris through kerbside in CI.`

### Brief 6: documentation

Update the documentation for what steps 1 to 5 shipped. Follow the
repository's documentation rules in `AGENTS.md`: `docs/` holds the
detail, README is a pitch, and ARCHITECTURE and AGENTS change only
for shape and convention.

- **`docs/xrdp.md` *Installing*.**
  - Lead with `pipx install andris` (or `pip install --user andris`),
    then `andris install-unit`, then the `enable --now` line
    (since step 7, `enable` and `restart`).
  - Keep the build-from-source route second, with
    `install-unit` working for it too.
  - Fold *Configuring*'s ticket recipe into "install-unit makes one
    if none exists".
  - Say that a reinstall elsewhere needs `install-unit` again, and
    that local changes go in a drop-in.
- **`docs/running.md`.**
  - *Building*: libpulse is optional at run time; without it andris
    serves without sound and says so.
  - *Patents*: what the wheels carry (decision 1).
  - *Connecting with ryll*: ryll 0.3.0 or newer (finding 8).
  - *Fronting andris with kerbside*: add *Trying it with kerbside's
    demo stack*, the recipe of decision 10. Before writing that
    recipe, confirm in kerbside's `config.py` and its demo that the
    SPICE, redirect and API listeners can all be bound to
    `127.0.0.1` under host networking. Write the confirmed form, and
    say in the commit message which form was used.
- **`docs/development.md`.** Cover:
  - `make wheel`, `make e2e-kerbside` and the kerbside pin bump;
  - the two new merge-tier jobs, in the tier and gate tables;
  - the release workflow in *Other workflows*, linking
    `docs/releasing.md`.
- **README.** Make `pipx install andris` the install instruction,
  and add the releasing link.
- **ARCHITECTURE.md.** Add one line on packaging, the wheel and
  `install-unit`, if it has a section where that belongs; otherwise
  nothing.
- **AGENTS.md.** Step 1 already changed the `unsafe` rule. Add
  nothing else unless a convention changed.

Check: `grep -rn "install -m 755\|/usr/local/bin" docs README.md`
leaves only the source route. No page states the install steps
differently from `docs/xrdp.md`.

Commit subject: `Document installing andris from PyPI.`

### Brief 7: operator check of the wheel

With the operator present:

1. Build the x86_64 wheel (`make wheel`).
2. The operator `pipx install`s it, then runs `andris install-unit`
   and `systemctl --user daemon-reload`.
3. The operator restarts `andris@10`, connects ryll directly over
   the existing SSH tunnel, and uses the desktop for a few minutes,
   with a video playing for sound.
4. Record the results:
   - `systemctl --user show andris@10 -p ExecStart`, which names the
     pipx binary;
   - the journal: no `status=31/SYS`, MIT-SHM chosen, sound
     captured;
   - the session-end metrics line.

The operator decides whether to keep the old `/usr/local/bin/andris`
until step 10. The management session reads only the journal and
`systemctl show`, never the display.

### Brief 8: push audit and pull request

Run `PUSH-AUDIT.md` over the branch against `develop`. Fix or decline
each finding in writing, in the Result section. Push the branch and
open the pull request. Handle the reviewer and CI per the session
relay. Merging and enqueueing stay with the operator.

### Brief 9: GitHub and PyPI settings (operator)

The operator, not an agent, sets these up. Use the values that step
4's `docs/releasing.md` gives:

- **On pypi.org**, a pending trusted publisher for the new project:
  - project `andris`;
  - owner `shakenfist`;
  - repository `andris`;
  - workflow `release.yml`;
  - environment `release`.
- **On GitHub**, an environment `release` with the operator as
  required reviewer.
- **On GitHub**, a tag ruleset protecting `v*`, as ryll's has.

### Brief 10: release 0.1.0

Do this after step 8's pull request merges and step 9 is done:

1. Run `make propose-release 0.1.0`, then open the release pull
   request (this plan step is the operator's authority to open it).
2. The operator merges it.
3. The operator runs `make tag-release 0.1.0`, or tells the session
   to. It is irreversible: once PyPI publishes, a version cannot be
   reused.
4. The operator approves the `release` environment.
5. Check that:
   - `pip download andris==0.1.0 --no-deps` fetches both wheels;
   - the GitHub release has both wheels and `SHA256SUMS`;
   - the tag verifies with gitsign.

### Brief 11: the soak (operator)

For seven calendar days, with at least five working days of use:

- **Setup.** The operator replaces the step 7 wheel with
  `pipx install andris==0.1.0`, and re-runs `andris install-unit`.
  They bring up kerbside's demo stack by the recipe in
  `docs/running.md`, and use ryll → kerbside → andris as the daily
  connection, with RDP available.
- **Each day, record** in the Result section:
  - hours used, and times RDP was needed, with why;
  - restarts of `andris@10` and their exit statuses, from
    `journalctl --user -u andris@10`, including any `status=31/SYS`;
  - frame rate and bandwidth from the session-end metrics lines;
  - anything that misbehaved.

Every regression gets an issue, in andris, ryll or kerbside as
appropriate. The management session reads only the journal and
metrics lines the operator points it at, and never the display.

### Brief 12: the acceptance pull request

On a branch `x11-desktop-phase-10-acceptance`, in a new worktree:

1. Record the soak in this plan's Result section, and mark the
   master plan's success criteria that the soak settles.
2. Act on decision 6's `SystemCallFilter` result.
3. Land any fixes made for regressions, unless they already landed
   in their own pull requests, and move the rest to the master
   plan's Future work.
4. Run the push audit over this pull request, then push it and open
   it.

**Remind the operator to upgrade their local andris install** if
step 11 did not already: `/usr/local/bin/andris` is the 2026-10-17
build from before phase 9's audit, and should be removed once the
pipx install and `install-unit` are serving `andris@10`. This is also
plan-blockers note n1 on the master plan.

## Risks and mitigations

- **Maturin's built-in policy check may not fail on an out-of-policy
  library the way decision 2 expects.** Step 3's first mutation shows
  whether it does. If not, step 3 adds ryll's explicit `DT_NEEDED`
  check with an empty exception list.
- **The `libpulse` bindings could be subtly wrong**, for example in
  an enum value or a struct's padding, and fail only on a real
  server. Step 1 checks every constant and layout against
  `libpulse-sys`, and `andris/tests/pulse.rs` runs against a real
  daemon in `make e2e`. The management session reads `ffi.rs` line
  by line.
- **The kerbside stack may need egress at run time**, or refuse an
  internal network. Brief 5 stops for the management session rather
  than opening the network. The fallback is a network with egress
  but no published ports, which the management session approves in
  writing in the Result section.
- **Kerbside's demo glue at a pinned commit may drift from released
  kerbside on PyPI.** Kerbside's own compose file warns that this
  pairing is untested in its CI. The lane is that test, for andris,
  and a failure names kerbside's own log. Step 5 pins a tag's commit,
  not `develop`.
- **`SystemCallFilter` may kill andris on the operator's desktop
  mid-day.** `Restart=on-failure` brings it back in about two
  seconds, and RDP is the fallback. The soak counts these
  (decision 6).
- **A PyPI release cannot be undone.** Steps 9 and 10 are the
  operator's. Step 7's check of the wheel and the merge-tier smoke
  test come first, so 0.1.0 is not the first time the wheel runs.
- **The aarch64 wheel is untested** beyond building and maturin's
  policy check. `docs/releasing.md` says so, and the guarded dispatch
  of decision 8, if allowed, lets it be built before a tag.

## Definition of done

- **The tag is earned.** `make wheel` passes without
  `--skip-auditwheel`, and `unzip -p` of the wheel's binary through
  `readelf -d` lists no library outside the `manylinux_2_28` policy.
  Step 3's mutation shows the build failing with libpulse linked.
- **SIMD is in.** The x86_64 wheel's build log shows the SIMD check
  passing, and the mutation shows it failing without nasm.
- **No libpulse, no crash.** The merge-tier smoke test passes in
  `python:3-slim`, including `--platform synthetic` and the libpulse
  warning.
- **`install-unit` works from a wheel.** The smoke test's rendered
  `ExecStart` names the installed binary. Step 7's `systemctl show`
  output is in the Result section.
- **Kerbside carries every channel.** `Through kerbside` passes on
  the merge queue. Its log shows "hypervisor connection successful"
  for playback, and an H.264 stream and Opus packets arriving
  through the proxy.
- **Released.** `pip download andris==0.1.0 --no-deps` works for both
  architectures, and the tag verifies.
- **The audit is clean.** Development's `audit-check.py` reports no
  failures for andris.
- **The soak happened.** The Result section has seven days of
  entries, every regression found has an issue linked there, and
  `SystemCallFilter`'s fate is recorded.
- **Documentation agrees.** No page gives install steps that differ
  from `docs/xrdp.md`. `grep -n undecided docs/running.md` finds
  nothing about patents. `grep -n 0.1.7 docs/running.md` finds
  nothing.
- **Deferred work is tracked.** Future work has:
  - a `.deb`/`.rpm`;
  - crates.io versions of ryll's crates once a release contains
    #578;
  - anything the soak found that was not fixed.
- **The operator was reminded** to retire `/usr/local/bin/andris`
  (brief 12).

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work you
intend to do aligns with that plan.

## Result

### Step 7: the wheel on `:10`

On 2026-10-18 the operator installed step 3's x86_64 wheel with pipx,
ran `andris install-unit` and `systemctl --user daemon-reload`, and
removed `/usr/local/bin/andris`, all while connected through ryll. The
session did not drop, because nothing had restarted `andris@10`. The
old process (started 08:13, `NRestarts=0`) kept serving from its
deleted file: `/proc/<pid>/exe` read `/usr/local/bin/andris
(deleted)`, while systemd's `ExecStart` already named the pipx binary.
The operator had followed `install-unit`'s printed `systemctl --user
enable --now andris@10`, and `--now` does nothing to a running
instance.

That was a defect in step 2, fixed in this step: `install-unit` now
prints `enable` followed by `restart`. Restarting starts a stopped
instance, and replaces a running one, so the same commands serve a
first install, a replaced unit, and an unchanged unit whose binary
`pipx upgrade` replaced in place. `docs/xrdp.md` explains why, and
gains the upgrade recipe. The `next_steps` tests now require `restart`
and refuse `--now`. Mutating the output back to `enable --now` fails
both.

After the operator restarted `andris@10` at 16:40:39, the management
session read, from `systemctl show` and the journal only:

- `ExecStart` path `/home/mikal/.local/share/pipx/venvs/andris/bin/andris`,
  with `--ticket-file /home/mikal/.config/andris/ticket` from `%E`;
  `/proc/<MainPID>/exe` is the same pipx binary. `SystemCallFilter`
  is in effect.
- The startup log: `andris 0.0.1`, sound from `@DEFAULT_MONITOR@`
  (libpulse loaded at run time), `capturing the screen with MIT-SHM,
  through a shared memfd`, the desktop 2212x1308.
- ryll reconnected within a second. The playback channel opened in
  Opus mode with `capture_opened=true`. A video played: one H.264
  stream (1216x704, target 2568 kbit/s, about 2.5 Mbit/s, encode
  3 to 10 ms) and Opus at 105 kbit/s, with no server or audio drops.
  The operator reported that video playback works well.
- No `status=31/SYS`, nor any other kill, in the first minutes under
  the seccomp filter. The soak (step 11) decides whether it stays.
- The old binary's session-end line, as a baseline: 21840.5 s, 4.8
  fps, display 338 kbit/s, capture 0.9 ms, encode 1.1 ms, 929890 kB
  written.

The cursor warning, `hot spot (1, 1) is outside a 1x1 cursor`,
appears at startup with both the old binary and the new one, so it is
not a regression from packaging.

### Step 8: push audit

`PUSH-AUDIT.md` ran over `develop...HEAD` on 2026-10-18. Wave 1
(`pre-commit run --all-files`, `make lint`, `make test`) passed on the
step 7 tip. Wave 2's five reviewers found nothing blocking, critical
or high. Style conformance passed outright.

Taken, in one commit, each verified as noted:

- **Security:**
  - The wheel image's toolchain is pinned: the manylinux base by
    digest for each architecture, Rust 1.99.1, and maturin 1.15.0
    with `--require-hashes`. Renovate tracks all three.
  - `install-unit` refuses to run as root, which would follow the
    user's symbolic links; the `root_is_refused` mutation fails.
  - `SystemCallArchitectures=native` sits beside `SystemCallFilter`.
  - `sign-release-tag.sh` checks gitsign against a sha256 held in the
    script, not the release's own `checksums.txt`.
  - `persist-credentials: false` on the jobs that never push.
  - The wheel build fails on an RPATH or RUNPATH; a build linked with
    `-rpath,/tmp` fails it.
- **Tests:**
  - `tests/cli.rs` runs `install-unit` through the real binary, with
    `DISPLAY` cleared and a temporary config home, on its success
    path and a refused config home.
  - `scripts/test-check-release-version.sh` gives
    `check-release-version.sh` eleven fixture cases, run by
    pre-commit; two mutations of the script fail it.
- **Code:** `e2e-kerbside.sh`'s `redact()` replaces each secret as
  literal text. sed's regular expression broke on `/`, and a fixture
  token `ab/c+d.e*f$1` now redacts exactly.
- **Documentation:** ARCHITECTURE.md maps the kerbside lane and links
  `docs/releasing.md`. `docs/development.md` says the running.md
  recipe's kerbside version moves with the Makefile's pin.

After the fixes, `pre-commit`, `make lint`, `make test`, `make wheel
ARCH=x86_64` with `scripts/wheel-smoke.sh`, and `make e2e-kerbside`
all passed.

Declined:

- **The tag ruleset rejects `sign-tag`'s force-push** (security,
  medium). Refuted: ryll's live ruleset has the same rules
  (`creation`, `deletion`, `non_fast_forward`, bypassed only by the
  admin role). Its v0.3.0 tag object is tagged by
  `github-actions[bot]`, so CI's re-signed tag was accepted.
- **Actions pinned by tag, not SHA** (security, medium). Ryll's
  release workflow does the same; that is a fleet decision for
  shakenfist/development's templates, not this branch.
- **Warning on a group- or other-writable binary directory**
  (security, low). pipx installs under the user's home, and the
  check would have to walk every parent directory.
- **Secrets in CI logs, the kerbside cache, Renovate on the static
  runner, self-review** (security, informational). Per-run secrets in
  a network-less stack; the rest are fleet template choices.
- **Duplication in `e2e.sh` and the wheel scripts, and comment
  length** (code quality, advisory). Deliberate shell ports of the
  Makefile, each saying so; the long comments carry the why
  (systemd's quoting rules, header citations).
- **`config.rs` at 993 lines** (advisory). Under the 1,500 line
  threshold; splitting `Cli` off is a separate change.
- **The remaining edge cases of `install-unit`** (tests, advisory),
  such as a directory at the unit path. These are each refused by a
  plain filesystem error, which the cli test's refused path
  exercises in kind.
- **README's `pipx install andris` before 0.1.0 is on PyPI**
  (documentation, advisory). The docs ship in the pull request that
  precedes the release; step 10 follows the merge.
- **`make wheel` and `make e2e-kerbside` in AGENTS.md** (advisory).
  `docs/development.md` covers them, and they hold no convention an
  agent could not read there.

The unit changed after step 7, so the operator re-runs `andris
install-unit` and restarts `andris@10` before the soak.

### Steps 9 and 10: the release

The operator set up PyPI's trusted publisher, the `release`
environment and the `v*` tag ruleset. Release pull request #29 merged
as `c0617e9`, and the release workflow for `v0.1.0` succeeded on
2026-10-19. On 2026-10-28 the management session checked that:

- PyPI has `andris-0.1.0-py3-none-manylinux_2_28_x86_64.whl` and
  `..._aarch64.whl`, both uploaded 2026-10-19 05:59 UTC;
- the GitHub release has both wheels and `SHA256SUMS`, and
  `sha256sum -c SHA256SUMS` passes for both;
- `gh attestation verify` passes for the x86_64 wheel;
- the tag object is `c020049`, re-signed by `github-actions[bot]`, and
  `gitsign verify-tag v0.1.0` with `docs/releasing.md`'s identity and
  issuer reports a good signature and a validated Rekor entry. A
  clone fetched before `sign-tag` force-pushed still holds the
  unsigned tag, and needs `git fetch --force` on the tag to verify it.

### Step 11: the soak

The soak ran from 2026-10-19 to 2026-10-27 on andris 0.1.0 from PyPI,
installed with pipx and served by `andris@10`. **It did not go through
kerbside.** ryll reached andris through an SSH tunnel to
`127.0.0.1:5900`, and kerbside's demo stack was never brought up on
the development host. Kerbside's path is covered only by the
`Through kerbside` merge-tier lane, and the master plan's success
criteria and Future work say so.

RDP stayed available and was never needed. The operator reported no
misbehaviour beyond the issues below.

Hours are connected time, from the session-end metrics lines, which
the operator accepted as the measure. ryll stays connected around the
clock, and its sessions break at about 08:00 and 18:20 on working
days, so the daytime session is the working day. Rates are each
session's averages. Times and dates here and in the issues below are
local (AEDT, UTC+11).

| Day | Daytime session | fps | Display kbit/s | Notes |
|-----|-----------------|-----|----------------|-------|
| Mon 10-19 | 10:42–18:01, 7.3 h | 2.9 | 168 | Still the step 7 wheel (0.0.1). At 10:29 one start exited `status=2/INVALIDARGUMENT`: a trial `--listen 192.168.1.7:5900` was refused for lacking TLS, as designed. `andris@10` was restarted onto 0.1.0 at 18:06. |
| Tue 10-20 | 11:14–18:01, 6.8 h | 3.9 | 559 | The overnight session (17.1 h) carried 77 s of Opus. From 11:09 to 11:14, eight sessions lasted under a minute: two ryll instances replacing each other (#36). The operator restarted `andris@10` at 11:13. |
| Wed 10-21 | 08:10–18:23, 10.2 h | 3.7 | 490 | |
| Thu 10-22 | 08:08–18:20, 10.2 h | 2.5 | 211 | |
| Fri 10-23 to Sun 10-25 | one session, 10-22 18:20 to 10-26 08:47, 86.4 h | 3.6 | 273 | 33 H.264 streams and 42,401 Opus frames (about seven minutes of sound), no stream, server or audio drops. With no session boundary, the journal cannot separate Friday's use from the weekend. |
| Mon 10-26 | 08:54–16:32, 7.6 h | 2.4 | 106 | |
| Tue 10-27 | 08:21–17:55, 9.6 h | 2.4 | 116 | The operator restarted `andris@10` at 08:18, still on 0.1.0. |

Across the week:

- `NRestarts` stayed at 0. Every stop was a `SIGTERM: shutting down`
  from a deliberate restart, and the only failed start was the
  configuration error on 10-19.
- **No `status=31/SYS`**, nor any other signal, with MIT-SHM capture
  throughout and sound in two sessions.
- No server, stream or audio drops in any session-end line. Capture
  stayed at 0.4 to 1.0 ms and encode at 0.2 to 1.2 ms in sessions over
  ten minutes.
- One warning in the week: the client's resize request, refused by
  xorgxrdp's RandR, as phase 7 expects.
- `andris@10` used 4 h 4 min of CPU between 10-20 11:13 and 10-27
  08:18, about 2.5% of one core averaged over 165 hours of use and
  idle. This is not the success criterion's static-screen figure,
  which the soak did not measure.

Issues:

- **#36**, found on 10-20: a replaced client was dropped silently, so
  auto-reconnecting clients fought over the session. Fixed by #41,
  merged 10-25.
- #22 (inputs channel closed after Enter, the cause logged only at
  debug) and #26 (a stalled X request served a silent session) were
  filed before the soak, both on 10-17. #46 and #47 fixed them
  during it, both merged 10-27.

None of the three fixes is in 0.1.0. They reach the development host
with the next release. Nothing the soak found is left unfixed.

### Step 12: the acceptance pull request

- **`SystemCallFilter=@system-service` stays.** Decision 6's test
  passed: zero `status=31/SYS` over the week, with MIT-SHM and sound
  in use. The unit is unchanged.
- No fixes land here. Every regression's fix landed in its own pull
  request.
- The master plan's success criteria record what the soak settled,
  and its Future work gains a soak through kerbside.
- `/usr/local/bin/andris` was removed on 2026-10-18 (step 7), and the
  development host runs 0.1.0 from PyPI (`pipx list` shows no
  `direct_url.json` for it), so the operator's reminder is already
  acted on. Plan-blockers note n1 on the master plan can be cleared.

The Definition of done's evidence, for the items earlier sections do
not already carry. The CI evidence is #28's passing merge-queue run,
39117589930, on 2026-10-18:

- **The tag is earned.** `readelf -d` of the released 0.1.0 wheels'
  binaries lists only `libstdc++`, `libdl`, `libgcc_s`, `libpthread`,
  `libm`, `libc` and the dynamic loader, for both architectures, with
  no RPATH or RUNPATH. The merge-queue `Build the wheel` job prints the
  same `DT_NEEDED` list. Step 3's commit, `4248df1`, records the
  libpulse and nasm mutations.
- **SIMD is in.** The same job logs `openh264's SIMD code is in: ...
  defines WelsSampleSad16x16_sse2`.
- **No libpulse, no crash.** The same job's smoke test logs `andris
  --platform synthetic listened, and exited 0 on SIGTERM`.
- **Kerbside carries every channel.** `Through kerbside` logs that
  kerbside relayed the main, display, inputs, cursor and playback
  channels. Through the proxy, one H.264 stream decoded 73 frames
  without failures, and 599 Opus packets decoded at 48 kHz. Kerbside's
  log phrasing differs from the Definition of done's "hypervisor
  connection successful". The lane's own assertion is the check.
- **The audit is clean.** No consistency-audit issue is open against
  andris.
- **Documentation agrees.** `grep -n undecided docs/running.md` finds
  nothing. `grep -n 0.1.7 docs/running.md` finds one line, the warning
  that ryll 0.1.7 and earlier cannot decode andris's LZ4. That line
  recommends 0.3.0, which is what the check was guarding.
