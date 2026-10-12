# Phase 6: Sharing the xrdp session

Master plan: [PLAN-x11-desktop.md](PLAN-x11-desktop.md).

Planning effort: medium, as the master plan asks. Most of this phase
follows patterns andris already has: a signal handler, a systemd unit,
documentation. The parts that need judgement are how to test against
xrdp without touching the operator's live session, and what happens to
input when two remote-desktop servers inject into the same X server.
Those parts are given high-effort steps. The planning itself was done at
medium effort.

## Scope

In scope, as one andris pull request built up in steps:

- **A disposable xrdp test bed.** A container runs xrdp, xorgxrdp and a
  FreeRDP client against each other, so andris can share a real xorgxrdp
  session with a real RDP connection. Every test that resizes, logs
  out, kills or types into an xrdp session runs here, never on the
  host's `:10` (decision 1).
- **Interleaving checks** on the test bed, written as a runnable script
  rather than as notes:
  - keys from RDP and from SPICE, alternated;
  - lock keys;
  - an RDP resize, disconnect and reconnect while a SPICE client stays
    connected;
  - an RDP reconnect with another keyboard layout;
  - the session logging out.
- **Process lifetime.** Andris exits when the X display goes away, with
  a non-zero status and a log line saying why. It shuts down cleanly on
  SIGTERM as well as SIGINT.
- **Whatever the interleaving checks show andris must change.** This
  step is gated on step 2's results, and may turn out to be empty.
- **A systemd user unit**, a template instance per display
  (`andris@10.service`). It finds the session's display from its
  instance name, and `XAUTHORITY` from the usual default.
- **Documentation:** a new `docs/xrdp.md` covering the operator setup,
  plus updates to `running.md`, `docs/index.md`, the README's links,
  `AGENTS.md` and `ARCHITECTURE.md`.
- **Operator acceptance on `:10`, by hand:** the unit enabled for the
  real session, with RDP and ryll typing into it in turn.

Out of scope:

- **Resize and clipboard from the SPICE client.** These are phase 7.
  This phase checks only that an RDP resize reaches a SPICE client.
- **The `.deb`.** It is phase 10. This phase writes the unit file and
  documents installing it by hand. The unit runs `andris` from the
  standard path, so the `.deb` can ship the same file unchanged
  (decision 4).
- **Running the test bed in CI** (decision 2). It becomes Future work.
- **Starting the desktop.** Andris still attaches and never owns. When
  the X server goes, andris exits; it does not wait for a new one
  (decision 3).
- **Any change to ryll.** None is expected. If the test bed shows a
  ryll gap, it gets an issue, as in earlier phases.

## What the survey found

The master plan's phase 6 section was checked against the tree at
`81ebc91` (develop, after #8, #11 and two Renovate merges) and against
the development host. Its false or incomplete claims are corrected at
their source in this planning commit: the master plan's phase 6 and
phase 10 sections, and the `index.md` row.

1. **Andris does not exit when the X server goes away.**
   - `andris::run` (`andris/src/lib.rs:56-90`) serves until Ctrl-C and
     nothing else.
   - When the X connection drops, the X11 damage thread marks the
     `DamageHub` gone (`andris/src/platform/x11/damage.rs:168`), and
     the display channel closes (`server/display.rs`).
   - Nothing reaches the process, though. The listener keeps accepting
     connections, and every display channel a new client opens closes
     at once.
   - Under systemd that is the worst outcome: the unit looks healthy
     and serves nothing.
   - `Platform` (`andris/src/platform/mod.rs:550-555`) has no way to
     say the desktop has gone, so step 3 adds one.
2. **Only SIGINT is handled.** `tokio::signal::ctrl_c()` is the only
   signal andris waits on (`lib.rs:78`).
   - systemd stops a unit with SIGTERM, whose default action kills
     andris at once.
   - That skips `ServerHandle::stop`, so the key and button releases
     that `running.md` promises on shutdown never happen.
   - `scripts/e2e.sh:222-223` stops andris with SIGINT only.
3. **The X server on `:10`:**
   - it runs as the operator: `Xorg :10 -auth .Xauthority -config
     xrdp/xorg.conf -noreset -nolisten tcp`;
   - its socket is `/tmp/.X11-unix/X10`;
   - its cookie is in `~/.Xauthority`, the default that x11rb reads
     when `XAUTHORITY` is unset;
   - its keycodes are `xfree86`, with layout `us` and model `pc104`.
   Package versions are xrdp `0.10.1-3.1+deb13u2` and xorgxrdp
   `1:0.10.2-1`, both from Debian 13.
4. **A systemd user manager runs inside the xrdp session**, through
   sesman's PAM session:
   - `Linger=no`;
   - `graphical-session.target` is inactive, because the desktop is
     XFCE, started from `~/.xsession` through `/etc/X11/Xsession`;
   - the manager's environment does have `DISPLAY=:10.0`, but only
     because `/etc/X11/Xsession.d/95dbus_update-activation-env` imports
     it. That happens late in session start, after `default.target`
     units have started.
   The host runs systemd 257, which supports `RestartSteps=` and
   `RestartMaxDelaySec=` (both added in 254). Since 239, it also looks
   up a bare `ExecStart=` name in the standard paths.
5. **The input paths are separate X devices.**
   - Andris injects through XTEST (`platform/x11/input.rs`).
   - xorgxrdp injects through its own input driver (`xrdpkeyb` and
     `xrdpmouse`).
   - Andris maps each key to a keycode by XKB key name, from a table it
     reads once at startup (`platform/x11/keymap.rs`).
   - `running.md` already says a layout changed later is not noticed.
   - Under XI2 the core keyboard takes each device's keymap as that
     device sends events. Whether an RDP reconnect that loads another
     layout through xorgxrdp leaves andris's table right is unknown;
     step 2 finds out.
6. **Lock keys.** `KEY_MODIFIERS` from the SPICE client makes andris
   toggle each lock key that differs from the desktop's
   (`server/inputs.rs`). An RDP client does the same with its
   synchronise event when it takes focus. The two will each set the
   desktop's locks to their own client's state. Step 2 confirms that,
   and the documentation says so.
7. **Resizes already work in principle.** Phase 5 handles a desktop
   that changes size, including one a capture sees before damage
   reports it (#11's tests). Nothing has yet tested it against a real
   xorgxrdp RandR resize. That check belongs on the test bed, not on
   `:10`.
8. **`docs/running.md` covers the manual setup only.** "Exporting an X
   display" (`running.md:58-100`) gives a `DISPLAY=:10` command line. It
   says nothing about a service, about logout, or about what RDP and
   SPICE do to each other. The README's quick start
   (`README.md:26`) has the same command line.
9. **The master plan's phase 10** says it ships "the systemd user
   unit". This phase writes the unit, and phase 10 packages it. That
   section is reworded.

Nothing else in the master plan's phase 6 section was wrong.

## Decisions

1. **Test against a containerised xrdp, never against `:10`.**
   - `:10` is the operator's working desktop. A test that logs it out,
     resizes it or types into it destroys work, or types into whatever
     window has focus.
   - The test bed is a Docker image built from the `andris-dev` image,
     so the andris and ryll binaries need no rebuilding. It adds Debian
     13's `xrdp`, `xorgxrdp`, `freerdp3-x11`, `xdotool` and `dbus-x11`
     at the same versions as the host.
   - Inside it, root runs `xrdp-sesman` and `xrdp` on loopback, and a
     user, `tester`, has a password.
   - `xfreerdp3` runs on an Xvfb of its own and connects. Sesman then
     starts a real xorgxrdp `Xorg :10` for `tester`, whose session runs
     the e2e keylogger instead of a desktop.
   - Andris and ryll headless run in the same container, as `tester`.
   - Keys reach the RDP side through `xdotool`, typing into the FreeRDP
     window. They reach the SPICE side through ryll's control socket,
     as `make e2e` already does.
   - If xrdp will not run in a container, step 1's fallback is a
     throwaway VM under `/dev/kvm`, with the same script driving it
     over SSH.
2. **The test bed runs locally (`make e2e-xrdp`), not in CI.**
   - What it guards is xrdp's behaviour, which changes when Debian
     updates xrdp, not when an andris pull request lands.
   - An RDP client driven through `xdotool` on a shared runner is the
     sort of check that flakes. A flaky required check costs more than
     the regressions it would catch.
   - Andris's own half, exiting when the display goes, is covered in
     CI by `make e2e` on Xvfb (step 3).
   - Moving the test bed into CI is recorded as Future work, with step
     2's measured run time as the evidence.
3. **When the X server goes, andris exits rather than waiting for a new
   one.**
   - The master plan says "exit cleanly, let systemd restart", and
     that is right. A new session has a new cookie, perhaps a new size
     and keymap, and a fresh process picks all of that up for free.
   - The exit status is non-zero: the desktop went away, which is not
     a clean stop. The log line names the cause, and the clients'
     connections are closed first.
   - SIGTERM and SIGINT both mean "stop", exit 0, and release held
     keys within the existing five-second grace.
   - Andris does not handle SIGHUP. Nothing sends it to a service.
4. **A template user unit, `andris@.service`, keyed by display number.**
   - `ExecStart=andris --display :%i --ticket-file
     %h/.config/andris/ticket $ANDRIS_ARGS`.
   - `EnvironmentFile=-%h/.config/andris/andris.env` supplies
     `ANDRIS_ARGS` (listen address, TLS) and, if needed, `XAUTHORITY`.
   - The bare `andris` resolves in systemd's standard search path
     (survey finding 4). The same file therefore works for a
     hand-installed `/usr/local/bin/andris` and for phase 10's `.deb`
     in `/usr/bin`.
   - Restarts back off: `Restart=on-failure`, `RestartSec=2s`,
     `RestartSteps=5`, `RestartMaxDelaySec=60s` and
     `StartLimitIntervalSec=0`.
   - It is `WantedBy=default.target`. Started before the session's X
     server exists, it fails to connect and retries until the server
     is there.
   - The file lives at `packaging/systemd/andris@.service`.

   This is the decision a reviewer is most likely to question. The
   alternative is one `andris.service` that reads `DISPLAY` from the
   manager's environment. It is the more usual shape for a desktop
   service, but on this host it is fragile:
   - `DISPLAY` arrives only when Xsession runs `95dbus_update-activation-env`,
     after `default.target` has started (survey finding 4).
   - The session in the manager's environment is whichever one last
     imported it.
   - Nothing starts `graphical-session.target` under XFCE, so the unit
     cannot be ordered after it.

   An explicit instance is deterministic, and survives a restart. An
   xrdp session keeps its display number, because `KillDisconnected`
   is false, so naming it once is no burden.
5. **Andris types through the desktop's current layout, as RDP does.**
   The SPICE client sends scancodes, and the X server's layout decides
   which characters they make. An RDP reconnect that switches the
   layout therefore changes what SPICE keys type, and that is
   intended. What must not happen is andris's scancode-to-keycode
   table going stale. Step 2 tests exactly that, and only if it fails
   does step 4 add a reload on `XkbNewKeyboardNotify` and
   `MappingNotify`.
6. **Lock-key state is "last client to synchronise wins", and is
   documented, not arbitrated.** Each remote-desktop protocol already
   synchronises the locks when its client takes focus, and either
   choice of winner would be wrong half the time.
7. **No step touches the host's real session except step 6.** Step 6
   runs with the operator present, starts nothing that types until the
   operator says so, and never logs out, resizes or kills `:10`.
   - No capture of `:10` is written to the repository or kept on disk.
     As in phase 5, it is the operator's desktop.
   - The unit is enabled for `:10` only with the operator's agreement,
     in step 6.

## Execution

Every step is a commit on the andris branch
`x11-desktop-phase-06-sharing`, in a worktree at
`/srv/kasm_profiles/mikal/vscode/src/shakenfist/andris-wt-p06`. The
management session makes each commit after reviewing the sub-agent's
diff.

- Every commit must leave `make lint` and `make test` passing.
- Cargo runs only through the `Makefile`, never on the host.
- No code is translated from spice-server, spice-gtk, x11spice or
  xrdp. Their behaviour is cited from the survey.
- Every step adds tests for what it adds.
- **No sub-agent sends a signal to, or injects input into, any process
  belonging to the host's `:10` session.** Its X server, sesman and
  chansrv are out of bounds.

| Step | Effort | Model | Isolation | Status | Brief for sub-agent |
|------|--------|-------|-----------|--------|---------------------|
| 0 | low | management | none | Complete | Commit the phase 5 close-out and this plan. See brief 0. |
| 1 | high | opus | none | Complete | The containerised xrdp test bed, with a smoke run. Gated. See brief 1. |
| 2 | high | opus | none | Complete | Interleaving checks as `make e2e-xrdp`, with their results recorded. Gates step 4. See brief 2. |
| 3 | high | opus | none | Complete | Process lifetime: SIGTERM, and exiting when the desktop goes. See brief 3. |
| 4 | high | opus | none | Not started | Fixes that step 2 calls for, if any. Gated. See brief 4. |
| 5 | medium | sonnet | none | Not started | The systemd user unit and the documentation. See brief 5. |
| 6 | medium | management | none | Not started | Operator acceptance on `:10`, by hand. See brief 6. |
| 7 | medium | management | none | Not started | Push audit, pull request, queue. See brief 7. |

Steps 1 and 3 touch different files and may run in parallel. Step 2
needs step 1, step 4 needs step 2, and step 5 needs steps 3 and 4.

### Brief 0: commits

Make two commits, with the management session's permission:
- the phase 5 close-out in the master plan and `docs/plans/index.md`;
- this plan, with the corrections to the master plan's phase 6 and
  phase 10 sections.

### Brief 1: the xrdp test bed

**Goal:** a container in which a real xrdp session exists that andris
can attach to, set up by one command, with a smoke check.

**Files:**
- `scripts/xrdp-testbed/Dockerfile`;
- `scripts/xrdp-testbed/run.sh`, which runs inside the container;
- `scripts/e2e-xrdp.sh`, which runs on the host;
- a `Makefile` target `e2e-xrdp`, beside `e2e`;
- the image name `andris-xrdp-testbed`.

**What to build:**
- **The image** builds `FROM andris-dev`, the devcontainer image the
  `Makefile` already builds, so its glibc matches the andris binary and
  ryll headless.
  - Add `xrdp`, `xorgxrdp`, `freerdp3-x11`, `xdotool`, `dbus-x11` and
    `python3-xlib`.
  - Check the installed xrdp and xorgxrdp versions against survey
    finding 3, and print them in the run's output.
- **The run script**, as root in the container:
  1. Create user `tester` with a fixed password. This is a throwaway
     container on `--network none`, so the password is not a secret;
     say so in a comment.
  2. Give `tester` a `~/startwm.sh`, which xrdp runs as the session,
     that starts `scripts/e2e-keylogger.py` and nothing else.
  3. Start `xrdp-sesman` and `xrdp`, listening on loopback.
  4. Start an `Xvfb :1` for the client, and run `xfreerdp3 /v:127.0.0.1
     /u:tester /p:... /cert:ignore /size:1280x800
     /dynamic-resolution` on it.
  5. Wait for sesman's Xorg (expect `:10`, and read the number from
     sesman's log rather than assuming it) and for the keylogger's
     window to map.
  6. As `tester`, with `XAUTHORITY=~tester/.Xauthority`, start andris on
     that display with a ticket file, and ryll headless against andris
     on loopback.
  7. Smoke checks:
     - andris logs which capture method it chose: MIT-SHM is expected,
       because the display is local;
     - all four channels connect;
     - ryll's screenshot has the session's size.
- **The host script** builds the image if needed and mounts the
  worktree read-only, the release andris binary and the ryll headless
  binary (`make ryll-headless`). It runs `run.sh` and propagates its
  exit status. Mirror how `scripts/e2e.sh` finds the binaries and
  cleans up.

**Gate.** If xrdp, sesman or xorgxrdp will not start in a container
within about two hours of effort, stop and report what blocked it.
Common causes are PAM, a missing `/dev/shm`, `setuid`, and a container
running without an init. The management session then decides between
the VM fallback (decision 1) and narrowing the phase.

**Do not** connect to, signal or read from the host's `:10`, or run
anything against the host's xrdp.

### Brief 2: interleaving checks

**Goal:** turn the open questions about sharing into assertions that
run on the test bed, and record what they find.

**Files:**
- `scripts/e2e-xrdp-drive.py`, which `run.sh` calls after the smoke
  check;
- `scripts/e2e-keylogger.py` may grow, for example to log each key's
  keysym and the lock state.

Each check prints PASS or FAIL with what it saw. The run fails on any
FAIL.

| Check | What it does | Expected |
|-------|--------------|----------|
| X1 | Type `a`, `b`, `c`, `d`, alternating RDP (`xdotool` into the FreeRDP window) and SPICE (ryll's control socket); repeat 20 times | The keylogger sees every key once, in order, with the right keysym |
| X2 | Hold Shift on the RDP side and type `x` over SPICE, then the other way round | Record what happens; do not assume. The core keyboard merges modifiers across devices, so `X` is the likely result |
| X3 | Toggle Caps Lock from RDP, then restart ryll headless, which sends `KEY_MODIFIERS` when its inputs channel opens | The lock state follows whichever client synchronised last (decision 6). Record it |
| X4 | Resize the FreeRDP window with `xdotool windowsize`, so xrdp resizes the session through RandR | Ryll gets a new surface of the new size, and its screenshot has that size |
| X5 | Kill FreeRDP, wait 5 s, start it again | Andris and ryll stay connected throughout. The session survives, because the container's `sesman.ini` keeps it, as the host's does. Keys from both sides still arrive |
| X6 | Reconnect FreeRDP with a German layout (`/kbd:layout:0x407`), then send SPICE scancodes for `y` and `z` | They arrive as the German layout's keysyms (`z` and `y`), from keycodes that are still right (decision 5) |
| X7 | End the xrdp session: have `startwm.sh` exit when told, or kill the session's Xorg | Before step 3, andris keeps running (survey finding 1); record this. After step 3 it must exit non-zero within 10 s, logging that the display has gone. Write the assertion so it expects the step 3 behaviour, and mark it expected-to-fail until then |

Record in this plan's Result section, as a table:
- each check's outcome;
- anything surprising;
- the time a whole `make e2e-xrdp` run takes, cold and warm.

If X1, X4, X5 or X6 fails, write down what step 4 must change and
stop. That is the gate.

### Brief 3: process lifetime

**Goal:** andris stops cleanly on SIGTERM, and exits with an error when
the desktop goes away.

**Code:**
- **A desktop-gone signal on `Platform`.** Add a field
  `gone: watch::Receiver<Option<String>>`, or a small type wrapping one,
  next to `geometry`, `screen`, `input` and `cursor`
  (`platform/mod.rs:550`). It carries why the desktop went.
- **The X11 implementation.** `X11Platform` keeps the sender. The damage
  thread already reaches `hub.gone(why)`
  (`platform/x11/damage.rs:160-175`) when the connection fails, so set
  the signal there.
  - Check the capture, input and cursor threads
    (`platform/x11/mod.rs:244-263`) for their own connection-failure
    paths. Any thread that finds the connection dead sets the signal
    too.
  - The signal is set once; the first reason wins.
- **The synthetic platform** gets `vanish(why)`, which sets the signal
  and marks its damage hub gone. Tests use it.
- **`andris::serve`** (`lib.rs:56`) selects over three things:
  - `ctrl_c()`;
  - SIGTERM, through `tokio::signal::unix::signal(SignalKind::terminate())`;
    tokio's `signal` feature is already on for `ctrl_c`;
  - the platform's gone signal.

  On either signal it logs `SIGINT: shutting down` or `SIGTERM: shutting
  down`, runs the existing `handle.stop(STOP_GRACE)`, and returns
  `Ok`.

  When the desktop goes, it logs an error with the reason. It stops the
  server with a short grace, because there is nowhere left to release
  keys, but the clients' connections must still close. Then it returns
  an `Err` naming the reason, so the process exits non-zero.

**Tests:**
- In-process, with the synthetic platform: after `vanish`, the serve
  future returns `Err` within a second, and a connected test client
  sees its connection closed.
  - If `serve` is not reachable from the tests, factor the wait out
    into a function the tests can drive with a fake signal source.
  - Do not deliver real signals inside `cargo test`.
- `scripts/e2e.sh`:
  - stop andris with SIGTERM instead of SIGINT, and keep asserting exit
    0 and the release log;
  - in a separate andris run (or after restarting andris), kill Xvfb
    and assert andris exits non-zero within 10 s, with the reason in
    its log.

  Keep the e2e job's added time under 5 s.
- Flip check X7 in `scripts/e2e-xrdp-drive.py` to a required PASS.

**Docs:**
- `running.md`'s Options paragraph names both signals.
- `running.md` says what happens when the X server goes away.

### Brief 4: fixes from the interleaving checks

Gated on step 2's record. The likely candidates, each only if a check
showed it was needed:
- **Reloading the keycode table.** If X6 or X1 failed because andris's
  table went stale, select `XkbNewKeyboardNotify` and `XkbMapNotify`
  (and core `MappingNotify`) on the input thread's connection, and
  rebuild the table from `GetNames` when they arrive.
  - Rebuild it on the input thread, so that no key is injected halfway
    through a rebuild.
  - Log the rebuild at info, naming the keycodes set.
  - Test it with the X11 test against Xvfb: change the keymap with
    `setxkbmap -keycodes`, then type.
- **Anything else** that X1, X4 or X5 shows. Brief it from the
  recorded failure, at the same effort.

If step 2 needs no fix, mark this step Skipped, giving the reason, in
the table and the Result section.

### Brief 5: the systemd unit and the documentation

**The unit.** `packaging/systemd/andris@.service`, as decision 4 says:

```ini
[Unit]
Description=Andris SPICE server for X display :%i
Documentation=https://github.com/shakenfist/andris/blob/develop/docs/xrdp.md
StartLimitIntervalSec=0

[Service]
EnvironmentFile=-%h/.config/andris/andris.env
ExecStart=andris --display :%i --ticket-file %h/.config/andris/ticket $ANDRIS_ARGS
Restart=on-failure
RestartSec=2s
RestartSteps=5
RestartMaxDelaySec=60s

[Install]
WantedBy=default.target
```

- Check it with `systemd-analyze --user verify` on the host. That is
  read-only, and does not need the unit installed.
- Install it under a throwaway instance on a throwaway display: an Xvfb
  on `:98`, enabled as `andris@98`, under the operator's user manager.
  - Check that it starts.
  - Check that killing the Xvfb gives a failed exit, then a restart
    that fails until the Xvfb is back, and then a running service
    again.
  - Check that `systemctl --user stop` exits 0.
  - Remove the instance afterwards.

  This touches the host's user manager but not `:10`.

**Docs:**
- **`docs/xrdp.md`**, "Running andris alongside xrdp", for an operator
  who has never read the plans. It covers:
  - installing the binary and the unit;
  - the ticket file and `andris.env`;
  - `systemctl --user enable --now andris@10`;
  - finding the display number;
  - `XAUTHORITY`;
  - what happens on logout and on RDP disconnect;
  - how RDP and SPICE input interleave, from step 2's results: lock
    keys, layouts and held modifiers;
  - `journalctl --user -u andris@10`;
  - a pointer to `running.md`'s kerbside section.

  No reference to plan phases.
- **`docs/running.md`:** link to `docs/xrdp.md` from "Exporting an X
  display".
- **`docs/index.md`:** add the new page.
- **README:** add an absolute link to the new page under its curated
  links. Nothing else in the README changes.
- **`AGENTS.md`:** one line saying that tests involving xrdp run on the
  test bed (`make e2e-xrdp`), never against a live session. It is a
  convention an agent cannot infer.
- **`ARCHITECTURE.md`:** one sentence saying the process exits when the
  platform reports the desktop gone.
- **`docs/development.md`:** `make e2e-xrdp`, what it needs and how
  long it takes.

### Brief 6: operator acceptance on `:10`

The management session runs this with the operator.

1. Install the release binary to `/usr/local/bin/andris`, and the unit
   and ticket under the operator's home.
2. With the operator's agreement, `systemctl --user enable --now
   andris@10`, then check the journal for the capture method and the
   desktop size.
3. The operator connects ryll from the Mac, directly or through
   kerbside. With RDP still connected, they type in a scratch window
   from each side in turn, and try Caps Lock from each.
4. Record the outcome in the Result section, as a sentence per check,
   with no screenshots.

The unit stays enabled afterwards only if the operator wants it; phase
10's soak is when it must be.

### Brief 7: push audit

Run `PUSH-AUDIT.md` over `origin/develop...HEAD` in two waves, as phase
5 did:
- wave 1: pre-commit, `make lint test e2e`, and `make e2e-xrdp`;
- wave 2: the reviewers.

Fix what is blocking, and record the declined findings in the Result
section. Then push and open the pull request when the operator asks.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| xrdp will not run in a container: PAM, `setuid`, `/dev/shm`, no init | Step 1 is time-boxed and gated. The fallback is a throwaway VM under `/dev/kvm`, driven by the same script over SSH (decision 1) |
| A test types into, resizes or logs out the operator's real session | The Execution rules forbid any step but 6 from touching `:10`. The management session checks every script's display handling in review. Every test-bed script refuses to run outside the container, by checking for a marker file the image creates |
| Interleaving checks flake on timing | Each check waits for the keylogger's output, with a deadline, rather than sleeping. A flaky check is fixed in step 2, not skipped |
| Exiting when the desktop goes drops a connected client abruptly | That is intended: the desktop is gone. The client sees the connection close, and reconnects once systemd has restarted andris against the new session. `docs/xrdp.md` says so |
| A restart loop while no session exists fills the journal | The unit backs off up to 60 s between attempts (decision 4) |
| SIGTERM changes `make e2e`'s shutdown and hides a regression in the SIGINT path | The in-process test drives both signals' shutdown through the same function. Ctrl-C stays documented, and step 3's brief keeps one e2e run on each signal if the time budget allows |
| The keycode table goes stale after an RDP reconnect (survey finding 5) | Check X6 detects it, and step 4 fixes it if so |

## Definition of done

- **The checks pass.**
  - `make lint test e2e` passes, and `make e2e-xrdp` passes locally.
  - The pull request passed its smoke tier, and `merge-build`, "End to
    end" and `Can merge` in the merge queue.
- **SIGTERM is a clean stop.** `scripts/e2e.sh` stops andris with
  SIGTERM, and asserts exit 0 and the release log.
- **A vanished display ends the process.**
  - `scripts/e2e.sh` kills Xvfb and asserts that andris exits non-zero
    within 10 s, with the reason in its log.
  - An in-process test does the same with `SyntheticPlatform::vanish`.
- **The interleaving checks are recorded.** The Result section has a
  row for each of X1 to X7, with its outcome, and the run time of
  `make e2e-xrdp`.
- **The unit is valid.** `systemd-analyze --user verify
  packaging/systemd/andris@.service` reports nothing, and the
  throwaway `andris@98` run in brief 5 is recorded.
- **Documentation.**
  - `docs/xrdp.md` exists, and is linked from `docs/index.md`, from
    `running.md` and from the README.
  - `grep -n "phase" docs/xrdp.md docs/running.md` finds nothing.
  - `running.md` names SIGTERM.
- **No step but 6 touched `:10`.** The Result section says so, and
  says what step 6 did.
- **Deferred work is tracked.** CI for the test bed is in the master
  plan's Future work. Every ryll gap found has an issue.

## Result

Filled in as the steps complete.

**Departures from the plan:**
- **Step 1 put the test bed's session on `:50`, not `:10`**, by setting
  `X11DisplayOffset=50` in the container's `sesman.ini`. A command aimed
  at the container's own `:10` reads, to a reviewer or to a permission
  check, exactly like one aimed at the operator's desktop. The number is
  still read from sesman's log rather than assumed.
- **Step 1 was built in a worktree of its own** and cherry-picked, so
  that its release build could not pick up step 3's code half-written.
  It also added `scripts/xrdp-testbed/smoke.py`, which reuses
  `scripts/e2e-drive.py`'s helpers for the control-socket checks.
- **Step 2 checks X7's exit status in `run.sh`, not in the drive
  script.** Andris is `run.sh`'s child, and bash reaps it as soon as it
  exits, so only `run.sh`'s `wait` can see its status. The drive script
  checks the rest of X7: the time to exit, and the log line.
- **Step 2 starts FreeRDP with `-grab-keyboard`.** FreeRDP 3 grabs the
  client display's keyboard while its window has the focus, which made
  it impossible to change the client's own lock keys without FreeRDP
  sending them on. The checks need that, to align the two clients' lock
  state before counting keys.

**Step 2: the interleaving checks.** `make e2e-xrdp` runs X1 to X7 after
the smoke checks (`scripts/e2e-xrdp-drive.py`). The keylogger gained a
`--detail` mode, which prints each key's keysyms, keycode and state, and
each `MappingNotify`; `make e2e` uses its old output unchanged. Before
X1 the lock keys are aligned (Num Lock on, Caps Lock off, which is what
ryll asserts), so that neither client's synchronisation toggles a lock
while keys are being counted.

| Check | Outcome | What was observed |
|-------|---------|-------------------|
| X1 | Pass | 80 keys, alternating RDP and SPICE, each arrived once, in order, with the right keysym. Every switch of source device sends the keylogger two `MappingNotify` events (keyboard, then modifiers): 158 in 80 keys. That is the core keyboard taking each device's keymap, as survey finding 5 expected, and is harmless to a client that refreshes its keymap on `MappingNotify`. |
| X2 | Pass | Modifiers merge across devices. Shift held over RDP made SPICE's x type `X` (state 0x11), and Shift held over SPICE made RDP's x type `X`. Once each side released its Shift, x from either side was unshifted: nothing stuck. |
| X3 | Pass, with a finding | An RDP Caps Lock turned the session's Caps Lock on. A restarted ryll's `KEY_MODIFIERS` turned it off again, since ryll synchronised last (decision 6). FreeRDP synchronises again whenever its window takes the focus, and then wins. **But an idle ryll turned an RDP-set Caps Lock off 10.0 s after its last key, in every run.** See the finding below. |
| X4 | Pass | Resizing the FreeRDP window to 1024x768 resized the session through RandR from 1280x800 to exactly 1024x768. Ryll got a 1024x768 surface, and a screenshot of that size. |
| X5 | Pass | With FreeRDP killed, ryll stayed connected through 20 status polls in 5 s, and andris and ryll kept their processes. FreeRDP reconnected to the same session, with the same keylogger. Its `/size:1280x800` resized the session back from 1024x768 to 1280x800, and ryll's surface followed. A key from each side arrived. |
| X6 | Pass | Reconnecting with `/kbd:layout:0x407` made xrdp load layout `de` into the running session (`setxkbmap -query`: `us`, then `de`). SPICE scancode 0x15 typed `z` and 0x2C typed `y`, from keycodes 29 and 52, the same keycodes RDP's y and z used. The XTEST device types through the new layout, and andris's table, keyed by XKB key name, needed no reload. Brief 4's keymap reload is not needed. |
| X7 | Pass | Ending the session's window manager (the keylogger) ended the session. Andris exited 0.1 s later with status 1, logging `the desktop has gone, so andris is stopping: the X11 damage thread lost its connection to the X display`. |

**Finding: ryll's idle keepalive undoes lock keys set over RDP.** Ryll
restates the last `KEY_MODIFIERS` it sent whenever its inputs channel
has been quiet for 10 s, as a keepalive. Andris applies every
`KEY_MODIFIERS` against the desktop's current locks, as spice-server
does, so the keepalive toggles back any lock another client changed.
With a ryll client connected and idle, a Caps Lock or Num Lock pressed
over RDP lasts about 10 s. In effect ryll always wins, not the last
client to synchronise. Ryll's source confirms it, and shows more: ryll
sends `KEY_MODIFIERS` once, as its inputs channel opens, with the fixed
value `NUM_LOCK` rather than the Mac's own lock state
(`shakenfist-spice-renderer/src/channels/inputs.rs:249`). Its keepalive
(`:390-398`, `:966-975`) restates that value, on the stated assumption
that "the guest sees no change". So, while ryll is connected, an idle
desktop is put back to Num Lock on and Caps Lock off every 10 s. This is not one of the gating checks, and the
choice of fix is the management session's:
- **In andris:** apply a client's `KEY_MODIFIERS` only when it differs
  from the last one that client sent, or is the first on its channel.
  A restatement then changes nothing, from ryll or from any other client
  that does the same.
- **In ryll (an issue):** the keepalive should not carry state, or should
  restate the state the server last reported. Andris never sends
  `KEY_MODIFIERS` when the desktop's locks change; spice-server does,
  and ryll ignores it for the keepalive.

Either way, `docs/xrdp.md` should say how locks and held modifiers
behave.

**Other test-side findings.** FreeRDP 3 overwrites the password in its
own `argv`, so the checks restart it with the command `run.sh` passes
them rather than reading `/proc`. In development, the first connection
to a restarted ryll's control socket was once reset while the new ryll
was starting, so the checks remove the old socket before starting ryll,
and retry a failed connection until their bound.

**Runs.** Seven consecutive `make e2e-xrdp` runs passed, each check with
the same observations every time. Warm (image and binaries current), a
whole run takes 26 to 27 s, of which the checks take about 20 s; 10 s of
that is X3 waiting for ryll's keepalive. Cold, with the test-bed image
rebuilt and the binaries already built, it takes 62 s. A rebuild of the
release andris adds about 30 s. `make e2e`, `make lint test` and
`pre-commit run --all-files` pass.

## Back brief

Before executing any step of this plan, please back brief the operator
as to your understanding of the plan and how the work you intend to do
aligns with that plan.
