#!/bin/bash
# The end-to-end test: andris exporting an Xvfb display, driven by ryll's
# headless client through its control socket. `make e2e` runs this inside
# the devcontainer with no network and a read-only checkout, so everything
# it writes (ticket, .vv, logs, the control socket) goes in a directory
# under /tmp.
#
# It starts Xvfb and first runs the X11 backend's own tests against its bare
# root window (andris/tests/x11.rs, through cargo test, which the Makefile
# has already built): damage from a drawing, the drawing captured through
# MIT-SHM and through GetImage, resizes through RandR, which leave the
# display at its starting size, and the clipboard both ways, with xclip,
# which leaves CLIPBOARD with no owner.
#
# It then starts, in order: scripts/e2e-keylogger.py, a full-screen window
# of a known colour that prints each key's keysym name; andris against that
# display, reporting which capture method andris chose; and ryll --headless
# against andris, through a .vv file because a password needs one.
# scripts/e2e-drive.py then asserts over ryll's control socket that a
# frame of the window arrived, that two keys reached it, that an idle
# desktop is silent for two seconds once it has settled, that a small
# change arrives as few draws, that clipboard text crosses both ways
# through andris's agent, small, over one agent chunk and over the INCR
# threshold, that a resize from ryll within Xvfb's -screen size is applied
# while one above it is refused, logged once, and that a moving picture
# from scripts/e2e-video.py becomes one H.264 stream that decodes, ends
# when the picture stops, and leaves the area exact. This script then
# checks ryll's log for all four channels, since nothing else uses the
# cursor channel, and stops ryll with SIGINT; the session-end metrics line
# must count the stream.
#
# A second ryll, started with --no-h264, then sees the same picture: the
# driver checks that no stream is created, and once that ryll is stopped,
# andris's session-end line for its session must count JPEG draws and no
# stream. Then andris is stopped with SIGTERM, as systemd stops it; both
# rylls and andris must exit 0, and andris's log must say SIGTERM stopped
# it.
#
# Last, a second andris is started against the same display, and Xvfb is
# killed under it. That andris must exit 1 within GONE_BOUND seconds,
# saying in its log that the desktop has gone and why.
#
# It fails if any step fails or times out, if any process exits before it
# is stopped, or if andris logged an ERROR. On failure it prints every log.
#
# Environment:
#   RYLL    the ryll binary, built by `make ryll-headless` (required)
#   ANDRIS  the andris binary (default target/debug/andris)
#   CARGO   the cargo that runs the X11 backend's test (default cargo)

set -euo pipefail

RYLL=${RYLL:?RYLL must name the ryll binary; run this through make e2e}
ANDRIS=${ANDRIS:-target/debug/andris}
CARGO=${CARGO:-cargo}
X_DISPLAY=:99
GEOMETRY=1024x768
PORT=5930

# Bounds, in seconds, on each wait.
XVFB_BOUND=10
KEYLOGGER_BOUND=10
ANDRIS_BOUND=10
STOP_BOUND=15
GONE_BOUND=10

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d /tmp/e2e.XXXXXX)
sock=$work/ryll.sock

declare -A pids=()
names=()
passed=false

say() {
    echo "[e2e] $*"
}

# True while the process is running. A zombie (exited but not yet reaped)
# counts as gone, which kill -0 alone would not.
alive() {
    local state
    state=$(sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | cut -d' ' -f1) || return 1
    [ -n "$state" ] && [ "$state" != Z ]
}

# start NAME COMMAND...: run COMMAND in the background, its output to
# $work/NAME.log.
start() {
    local name=$1
    shift
    "$@" >"$work/$name.log" 2>&1 &
    pids[$name]=$!
    names+=("$name")
    say "started $name (pid ${pids[$name]})"
}

# wait_until BOUND NAME DESCRIPTION COMMAND...: poll COMMAND until it
# succeeds. Fails after BOUND seconds, or at once if process NAME exits.
wait_until() {
    local bound=$1 name=$2 what=$3
    shift 3
    local deadline=$((SECONDS + bound))
    until "$@" >/dev/null 2>&1; do
        if ! alive "${pids[$name]}"; then
            say "FAIL: $name exited while waiting for $what"
            return 1
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            say "FAIL: timed out after ${bound}s waiting for $what"
            return 1
        fi
        sleep 0.1
    done
    say "$what"
}

# stop NAME SIGNAL [any]: signal the process and wait for it, up to
# STOP_BOUND. Fails unless it exits 0, or with any status given `any`.
stop() {
    local name=$1 signal=$2 status=${3:-0} pid=${pids[$1]} rc=0
    kill "-$signal" "$pid"
    local deadline=$((SECONDS + STOP_BOUND))
    while alive "$pid"; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            say "FAIL: $name did not exit within ${STOP_BOUND}s of SIG$signal"
            return 1
        fi
        sleep 0.1
    done
    wait "$pid" || rc=$?
    if [ "$status" != any ] && [ "$rc" -ne 0 ]; then
        say "FAIL: $name exited $rc after SIG$signal"
        return 1
    fi
    say "$name exited $rc after SIG$signal"
}

# fails_within NAME BOUND: wait up to BOUND seconds for process NAME to
# exit by itself, which it must do with status 1, as andris returning an
# error does. A crash or a signal gives another status.
fails_within() {
    local name=$1 bound=$2 pid=${pids[$1]} rc=0
    local deadline=$((SECONDS + bound))
    while alive "$pid"; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            say "FAIL: $name did not exit within ${bound}s"
            return 1
        fi
        sleep 0.1
    done
    wait "$pid" || rc=$?
    if [ "$rc" -ne 1 ]; then
        say "FAIL: $name exited $rc, not 1"
        return 1
    fi
    say "$name exited $rc"
}

cleanup() {
    local name
    for name in "${names[@]}"; do
        if alive "${pids[$name]}"; then
            kill -KILL "${pids[$name]}" 2>/dev/null || true
        fi
        # Reaping here, with stderr closed, keeps the shell's "Killed"
        # notice out of the output.
        { wait "${pids[$name]}"; } 2>/dev/null || true
    done
    if ! $passed; then
        for name in x11-test "${names[@]}" drive drive-no-h264 video video-no-h264; do
            if [ -f "$work/$name.log" ]; then
                echo "===== $name.log ====="
                cat "$work/$name.log"
            fi
        done
        say "FAILED"
    fi
    rm -rf "$work"
}
trap cleanup EXIT

# Background jobs of a non-interactive shell ignore SIGINT, and ryll is
# stopped with it. Job control puts each in its own process group with
# default signal dispositions.
set -m

say "$("$RYLL" --version 2>&1 | head -n1 || true); $("$ANDRIS" --version 2>&1 | head -n1 || true)"

# 1. The X server. Not being root, Xvfb cannot create /tmp/.X11-unix and
# says so with an "ERROR" in its log; it listens on the abstract socket
# instead, which every client here uses.
start xvfb Xvfb "$X_DISPLAY" -screen 0 "${GEOMETRY}x24" -nolisten tcp
wait_until "$XVFB_BOUND" xvfb "Xvfb answering on $X_DISPLAY" xdpyinfo -display "$X_DISPLAY"

# 2. The X11 backend's test, while nothing else draws on the display.
say "running the X11 backend's test"
if ! ANDRIS_TEST_DISPLAY=$X_DISPLAY "$CARGO" test --frozen --quiet --test x11 \
    >"$work/x11-test.log" 2>&1; then
    say "FAIL: the X11 backend's test failed"
    exit 1
fi
say "the X11 backend's test passed"

# 3. The window that keys are typed into.
start keylogger python3 -u "$here/e2e-keylogger.py" --display "$X_DISPLAY"
wait_until "$KEYLOGGER_BOUND" keylogger "keylogger mapped" grep -qx READY "$work/keylogger.log"

# 4. The ticket, 0600 as andris requires.
ticket=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
(umask 077 && printf '%s\n' "$ticket" >"$work/ticket")

# 5. Andris. The display channel logs at debug, for the stream reports
# and bitrate changes the driver summarises.
start andris env RUST_LOG=info,andris::server::display=debug NO_COLOR=1 "$ANDRIS" --platform x11 \
    --display "$X_DISPLAY" \
    --listen "127.0.0.1:$PORT" --ticket-file "$work/ticket"
wait_until "$ANDRIS_BOUND" andris "andris listening on $PORT" grep -q "plain listener ready" "$work/andris.log"
say "andris is $(sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" | grep -o 'capturing the screen with [A-Za-z-]*' | head -n1 || true)"

# 6. Ryll, headless. The password needs a .vv file: --direct has no
# password option.
(umask 077 && printf '[virt-viewer]\ntype=spice\nhost=127.0.0.1\nport=%s\npassword=%s\n' \
    "$PORT" "$ticket" >"$work/andris.vv")
start ryll env NO_COLOR=1 "$RYLL" --headless --file "$work/andris.vv" --control-socket "$sock" --verbose

# 7. The assertions. The driver bounds its own waits, and gives up early
# if a process it depends on exits.
rc=0
python3 "$here/e2e-drive.py" --socket "$sock" --keylog "$work/keylogger.log" --display "$X_DISPLAY" \
    --andris-log "$work/andris.log" --ryll-log "$work/ryll.log" --work "$work" \
    --watch "andris=${pids[andris]}" --watch "ryll=${pids[ryll]}" --watch "keylogger=${pids[keylogger]}" \
    >"$work/drive.log" 2>&1 || rc=$?
cat "$work/drive.log"
if [ "$rc" -ne 0 ]; then
    say "FAIL: the driver exited $rc"
    exit 1
fi

# The log checks below send grep's output to /dev/null rather than
# using -q: grep -q exits at the first match, which can kill sed with
# SIGPIPE on a long log, and pipefail would turn that into a failure.

# Ryll opened every channel andris offers. The control socket has no
# per-channel status, so ryll's log is the oracle: SpiceClient's
# connect_channel logs "<channel>: connected successfully" once the
# ticket is accepted. Nothing above uses the cursor channel.
for channel in main display inputs cursor; do
    if ! sed 's/\x1b\[[0-9;]*m//g' "$work/ryll.log" | grep " INFO $channel: connected successfully\$" >/dev/null; then
        say "FAIL: ryll never connected the $channel channel"
        exit 1
    fi
done
say "ryll connected the main, display, inputs and cursor channels"

# Nothing may have exited before it was asked to.
for name in "${names[@]}"; do
    if ! alive "${pids[$name]}"; then
        say "FAIL: $name exited early"
        exit 1
    fi
done

# The newest session-end metrics line in andris's log.
last_session_end() {
    sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" | grep 'session ends after' | tail -n1
}

# 8. Stop ryll, which ends its session. Its metrics must count the
# stream the driver saw.
stop ryll INT
wait_until "$STOP_BOUND" andris "the first session's metrics line" \
    sh -c "sed 's/\x1b\[[0-9;]*m//g' '$work/andris.log' | grep -c 'session ends after' | grep -qx 1"
line=$(last_session_end)
if ! echo "$line" | grep 'streams created=1 destroyed=1,' >/dev/null; then
    say "FAIL: the first session's metrics do not count one stream created and destroyed: $line"
    exit 1
fi
say "the first session's metrics: ${line#*: }"

# 9. A ryll without H.264, and the same picture: no stream, JPEG draws.
start ryll-no-h264 env NO_COLOR=1 "$RYLL" --headless --file "$work/andris.vv" \
    --control-socket "$work/ryll-no-h264.sock" --verbose --no-h264
rc=0
python3 "$here/e2e-drive.py" --socket "$work/ryll-no-h264.sock" --display "$X_DISPLAY" \
    --andris-log "$work/andris.log" --ryll-log "$work/ryll-no-h264.log" --work "$work" --no-stream \
    --watch "andris=${pids[andris]}" --watch "ryll-no-h264=${pids[ryll-no-h264]}" \
    >"$work/drive-no-h264.log" 2>&1 || rc=$?
cat "$work/drive-no-h264.log"
if [ "$rc" -ne 0 ]; then
    say "FAIL: the --no-h264 driver exited $rc"
    exit 1
fi
stop ryll-no-h264 INT
wait_until "$STOP_BOUND" andris "the second session's metrics line" \
    sh -c "sed 's/\x1b\[[0-9;]*m//g' '$work/andris.log' | grep -c 'session ends after' | grep -qx 2"
line=$(last_session_end)
if ! echo "$line" | grep 'streams created=0 ' >/dev/null || ! echo "$line" | grep 'jpeg=[1-9]' >/dev/null; then
    say "FAIL: the --no-h264 session's metrics do not show JPEG draws and no stream: $line"
    exit 1
fi
say "the --no-h264 session's metrics: ${line#*: }"

# 10. Stop the rest but Xvfb. Andris is stopped as systemd stops a unit,
# with SIGTERM.
stop andris TERM
stop keylogger TERM any

# 11. Andris must not have logged an error. Colour is off, but strip any
# escape codes anyway so the match cannot be split by one.
if sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" | grep ' ERROR ' >/dev/null; then
    say "FAIL: andris logged an ERROR"
    exit 1
fi

# 12. Ending a session, which stopping each ryll did, logs the metrics.
if ! sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" | grep 'session ends after .*kB written in all' >/dev/null; then
    say "FAIL: andris never logged the session-end metrics line"
    exit 1
fi
say "andris logged the session-end metrics line"

if ! sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" | grep 'SIGTERM: shutting down' >/dev/null; then
    say "FAIL: andris never logged that SIGTERM stopped it"
    exit 1
fi
say "andris logged that SIGTERM stopped it"

# 13. The desktop going away: a second andris on the same display, and
# Xvfb killed under it.
start andris-gone env RUST_LOG=info NO_COLOR=1 "$ANDRIS" --platform x11 --display "$X_DISPLAY" \
    --listen "127.0.0.1:$PORT" --ticket-file "$work/ticket"
wait_until "$ANDRIS_BOUND" andris-gone "second andris listening on $PORT" \
    grep -q "plain listener ready" "$work/andris-gone.log"
killed=$(date +%s%N)
stop xvfb TERM any
fails_within andris-gone "$GONE_BOUND"
say "andris exited $(( ($(date +%s%N) - killed) / 1000000 ))ms after Xvfb was told to stop"
if ! sed 's/\x1b\[[0-9;]*m//g' "$work/andris-gone.log" \
    | grep 'the desktop has gone.*lost its connection to the X display' >/dev/null; then
    say "FAIL: andris did not log that the desktop had gone, and why"
    exit 1
fi
say "andris logged that the desktop had gone, and why"

passed=true
say "PASS"
