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
# It also starts a PulseAudio daemon of its own, unprivileged, whose only
# sink, and so its default, is a 48 kHz null sink, and exports PULSE_SERVER
# naming its socket, so that andris, pacat and pactl reach it and nothing
# else. With it up, the PulseAudio capture's own test runs
# (andris/tests/pulse.rs, which plays tones into that sink), before andris
# starts. The daemon is stopped last.
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
# when the picture stops, and leaves the area exact, and that a tone played
# into the null sink reaches ryll as one Opus stream that decodes and stops
# once the tone ends. This script then checks ryll's log for all five
# channels, since nothing else uses the cursor channel, and stops ryll with
# SIGINT; the session-end metrics line must count the stream, the audio
# frames, and one audio start and stop.
#
# A second ryll, started with --no-h264, then sees the same picture: the
# driver checks that no stream is created, and once that ryll is stopped,
# andris's session-end line for its session must count JPEG draws and no
# stream. A second andris, run with --no-audio on another port, must then
# give a third ryll no playback channel. Then andris is stopped with
# SIGTERM, as systemd stops it; the rylls and andrises must exit 0, and
# andris's log must say SIGTERM stopped it.
#
# Last, another andris is started against the same display, and Xvfb is
# killed under it. That andris must exit 1 within GONE_BOUND seconds,
# saying in its log that the desktop has gone and why.
#
# It fails if any step fails or times out, if any process exits before it
# is stopped, if either andris serving a session logged an ERROR, or if
# andris could not capture sound. On failure it prints every log.
#
# With E2E_VIA=kerbside, scripts/e2e-kerbside.sh runs it in the network
# namespace of kerbside's container, and ryll reaches andris through
# kerbside's proxy. Then andris listens on PORT with the lane's ticket
# file, and ryll is handed the lane's .vv, which kerbside minted, rather
# than one written here. The driver runs only the checks that cross every
# channel (frame, keys, clipboard, the H.264 stream and the tone), and the
# five channels and the first session's metrics are checked as before.
# The backend's own tests, the second ryll, the --no-audio andris and the
# Xvfb kill are skipped: they test andris, not the proxy, and the direct
# run covers them.
#
# Environment:
#   RYLL        the ryll binary, built by `make ryll-headless` (required)
#   ANDRIS      the andris binary (default target/debug/andris)
#   CARGO       the cargo that runs the X11 backend's test (default cargo)
#   PORT        the port andris listens on, on 127.0.0.1 (default 5930)
#   E2E_VIA     `kerbside` for the run through kerbside; unset otherwise
#   E2E_VV      with E2E_VIA=kerbside, the .vv ryll connects with
#               (required). It sets delete-this-file=1, so ryll removes
#               it, and its directory must be writable
#   E2E_TICKET  with E2E_VIA=kerbside, andris's ticket file, 0600, which
#               kerbside presents to andris (required)
#   E2E_LOGS    if set, a directory every log is copied into at the end,
#               pass or fail

set -euo pipefail

RYLL=${RYLL:?RYLL must name the ryll binary; run this through make e2e}
ANDRIS=${ANDRIS:-target/debug/andris}
CARGO=${CARGO:-cargo}
X_DISPLAY=:99
GEOMETRY=1024x768
PORT=${PORT:-5930}
VIA=${E2E_VIA:-direct}
case "$VIA" in
    direct) ;;
    kerbside)
        E2E_VV=${E2E_VV:?E2E_VV must name the .vv kerbside minted; run this through make e2e-kerbside}
        E2E_TICKET=${E2E_TICKET:?E2E_TICKET must name the lane ticket; run this through make e2e-kerbside}
        ;;
    *)
        echo "[e2e] E2E_VIA must be kerbside, or unset; got '$VIA'" >&2
        exit 1
        ;;
esac
# The port of the andris run with --no-audio, alongside the first.
NO_AUDIO_PORT=5931
# The null sink the PulseAudio daemon plays into, at the rate andris
# captures at. PulseAudio's default is 44.1 kHz.
SINK=e2e
# The fewest audio frames andris must count for the driver's tone, which
# is 500 frames of 10 ms: as many as the driver needs ryll to decode.
AUDIO_FRAMES=400

# Bounds, in seconds, on each wait.
XVFB_BOUND=10
PULSE_BOUND=10
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
    if [ -n "${E2E_LOGS:-}" ]; then
        cp "$work"/*.log "$E2E_LOGS"/ 2>/dev/null || true
    fi
    if ! $passed; then
        for name in x11-test pulse-test "${names[@]}" drive drive-no-h264 drive-no-audio video video-no-h264 tone; do
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

# 2. The X11 backend's test, while nothing else draws on the display. It
# tests andris's backend, not the proxy, so the run through kerbside skips
# it, as it does the capture's test below.
if [ "$VIA" = direct ]; then
    say "running the X11 backend's test"
    if ! ANDRIS_TEST_DISPLAY=$X_DISPLAY "$CARGO" test --frozen --quiet --test x11 \
        >"$work/x11-test.log" 2>&1; then
        say "FAIL: the X11 backend's test failed"
        exit 1
    fi
    say "the X11 backend's test passed"
fi

# 3. The sound server, unprivileged: a PulseAudio daemon with no
# configuration (-n), whose one sink, a null sink at 48 kHz, is the
# default. PulseAudio wants a private (0700) runtime directory and a
# writable home, so it gets its own of each under $work. Its socket is
# anonymous, since only this container's processes can reach $work, and
# PULSE_SERVER names it for every client from here on: andris, pacat and
# pactl, and the capture test. With PULSE_SERVER set, libpulse never
# spawns a daemon of its own.
pulse_runtime=$work/pulse-runtime
pulse_home=$work/pulse-home
mkdir -m 0700 "$pulse_runtime" "$pulse_home"
start pulse env HOME="$pulse_home" XDG_RUNTIME_DIR="$pulse_runtime" pulseaudio -n --daemonize=no \
    --exit-idle-time=-1 --use-pid-file=no --log-target=stderr --realtime=no --high-priority=no --disable-shm \
    --load="module-native-protocol-unix auth-anonymous=1 socket=$pulse_runtime/native" \
    --load="module-null-sink sink_name=$SINK rate=48000"
export PULSE_SERVER=unix:$pulse_runtime/native
wait_until "$PULSE_BOUND" pulse "PulseAudio answering" pactl info
pactl set-default-sink "$SINK"
say "PulseAudio's default sink is $(pactl get-default-sink)"
# Ryll plays what it decodes through ALSA, whose default device here is
# PulseAudio's plugin. Given this PULSE_SERVER, ryll would play the tone
# back into the null sink, andris would capture it again, and the sound
# would never stop. So each ryll gets a server that does not exist, and
# decodes with no device, as on a host without sound.
no_sound_server=unix:$work/ryll-has-no-sound-server

# The PulseAudio capture's test (andris/tests/pulse.rs), which plays
# tones into the default sink, so before anything else listens to it.
if [ "$VIA" = direct ]; then
    say "running the PulseAudio capture's test"
    if ! ANDRIS_TEST_PULSE=1 "$CARGO" test --frozen --quiet --test pulse >"$work/pulse-test.log" 2>&1; then
        say "FAIL: the PulseAudio capture's test failed"
        exit 1
    fi
    say "the PulseAudio capture's test passed"
fi

# 4. The window that keys are typed into.
start keylogger python3 -u "$here/e2e-keylogger.py" --display "$X_DISPLAY"
wait_until "$KEYLOGGER_BOUND" keylogger "keylogger mapped" grep -qx READY "$work/keylogger.log"

# 5. The ticket, 0600 as andris requires. Through kerbside it is the
# lane's, which kerbside's sources.yaml also holds.
if [ "$VIA" = kerbside ]; then
    ticket_file=$E2E_TICKET
else
    ticket=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
    ticket_file=$work/ticket
    (umask 077 && printf '%s\n' "$ticket" >"$ticket_file")
fi

# 6. Andris. The display channel logs at debug, for the stream reports
# and bitrate changes the driver summarises.
start andris env RUST_LOG=info,andris::server::display=debug NO_COLOR=1 "$ANDRIS" --platform x11 \
    --display "$X_DISPLAY" \
    --listen "127.0.0.1:$PORT" --ticket-file "$ticket_file"
wait_until "$ANDRIS_BOUND" andris "andris listening on $PORT" grep -q "plain listener ready" "$work/andris.log"
say "andris is $(sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" | grep -o 'capturing the screen with [A-Za-z-]*' | head -n1 || true)"

# 7. Ryll, headless. The password needs a .vv file: --direct has no
# password option. Through kerbside, the .vv is the one kerbside minted,
# which sends ryll to kerbside's TLS port with kerbside's token.
if [ "$VIA" = kerbside ]; then
    vv=$E2E_VV
    drive_mode=(--kerbside)
else
    vv=$work/andris.vv
    drive_mode=()
    (umask 077 && printf '[virt-viewer]\ntype=spice\nhost=127.0.0.1\nport=%s\npassword=%s\n' \
        "$PORT" "$ticket" >"$vv")
fi
start ryll env NO_COLOR=1 PULSE_SERVER="$no_sound_server" "$RYLL" --headless --file "$vv" \
    --control-socket "$sock" --verbose

# 8. The assertions. The driver bounds its own waits, and gives up early
# if a process it depends on exits.
rc=0
python3 "$here/e2e-drive.py" --socket "$sock" --keylog "$work/keylogger.log" --display "$X_DISPLAY" \
    --andris-log "$work/andris.log" --ryll-log "$work/ryll.log" --work "$work" "${drive_mode[@]}" \
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
for channel in main display inputs cursor playback; do
    if ! sed 's/\x1b\[[0-9;]*m//g' "$work/ryll.log" | grep " INFO $channel: connected successfully\$" >/dev/null; then
        say "FAIL: ryll never connected the $channel channel"
        exit 1
    fi
done
say "ryll connected the main, display, inputs, cursor and playback channels"

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

# 9. Stop ryll, which ends its session. Its metrics must count the
# stream the driver saw, and the tone: one audio start and stop, and at
# least as many audio frames as the driver needed ryll to decode.
stop ryll INT
wait_until "$STOP_BOUND" andris "the first session's metrics line" \
    sh -c "sed 's/\x1b\[[0-9;]*m//g' '$work/andris.log' | grep -c 'session ends after' | grep -qx 1"
line=$(last_session_end)
if ! echo "$line" | grep 'streams created=1 destroyed=1,' >/dev/null; then
    say "FAIL: the first session's metrics do not count one stream created and destroyed: $line"
    exit 1
fi
audio_frames=$(echo "$line" | sed -n 's/.*audio frames \([0-9]*\) at .*/\1/p')
if [ "${audio_frames:-0}" -lt "$AUDIO_FRAMES" ] || ! echo "$line" | grep 'audio starts=1 stops=1;' >/dev/null; then
    say "FAIL: the first session's metrics do not count $AUDIO_FRAMES audio frames and one start and stop: $line"
    exit 1
fi
say "the first session's metrics: ${line#*: }"

# Through kerbside, the rest tests andris rather than the proxy: stop
# andris as systemd does, and check its log, then stop the X server and
# the sound server.
if [ "$VIA" = kerbside ]; then
    stop andris TERM
    stop keylogger TERM any
    if sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" | grep ' ERROR ' >/dev/null; then
        say "FAIL: andris logged an ERROR"
        exit 1
    fi
    if sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" \
        | grep 'audio capture could not start\|cannot capture sound' >/dev/null; then
        say "FAIL: andris could not capture sound"
        exit 1
    fi
    if ! sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" | grep 'SIGTERM: shutting down' >/dev/null; then
        say "FAIL: andris never logged that SIGTERM stopped it"
        exit 1
    fi
    say "andris logged no ERROR, captured sound, and logged that SIGTERM stopped it"
    stop xvfb TERM any
    stop pulse TERM
    passed=true
    say "PASS"
    exit 0
fi

# 10. A ryll without H.264, and the same picture: no stream, JPEG draws.
start ryll-no-h264 env NO_COLOR=1 PULSE_SERVER="$no_sound_server" "$RYLL" --headless \
    --file "$work/andris.vv" --control-socket "$work/ryll-no-h264.sock" --verbose --no-h264
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

# 11. An andris without audio, alongside the first on another port: a
# ryll connected to it gets no playback channel. The driver checks ryll's
# stats, and ryll's log must show none offered and none linked.
start andris-no-audio env RUST_LOG=info NO_COLOR=1 "$ANDRIS" --platform x11 --display "$X_DISPLAY" \
    --listen "127.0.0.1:$NO_AUDIO_PORT" --ticket-file "$work/ticket" --no-audio
wait_until "$ANDRIS_BOUND" andris-no-audio "andris --no-audio listening on $NO_AUDIO_PORT" \
    grep -q "plain listener ready" "$work/andris-no-audio.log"
(umask 077 && printf '[virt-viewer]\ntype=spice\nhost=127.0.0.1\nport=%s\npassword=%s\n' \
    "$NO_AUDIO_PORT" "$ticket" >"$work/andris-no-audio.vv")
start ryll-no-audio env NO_COLOR=1 PULSE_SERVER="$no_sound_server" "$RYLL" --headless \
    --file "$work/andris-no-audio.vv" --control-socket "$work/ryll-no-audio.sock" --verbose
rc=0
python3 "$here/e2e-drive.py" --socket "$work/ryll-no-audio.sock" --display "$X_DISPLAY" \
    --andris-log "$work/andris-no-audio.log" --ryll-log "$work/ryll-no-audio.log" --work "$work" --no-audio \
    --watch "andris-no-audio=${pids[andris-no-audio]}" --watch "ryll-no-audio=${pids[ryll-no-audio]}" \
    >"$work/drive-no-audio.log" 2>&1 || rc=$?
cat "$work/drive-no-audio.log"
if [ "$rc" -ne 0 ]; then
    say "FAIL: the --no-audio driver exited $rc"
    exit 1
fi
# Ryll's debug lines list the channels andris offers, as "channel:
# playback (type=5, id=0)" for playback; the display's line shows that
# the list was logged.
if ! sed 's/\x1b\[[0-9;]*m//g' "$work/ryll-no-audio.log" | grep 'channel: display (type=2' >/dev/null; then
    say "FAIL: ryll's log does not list the channels andris --no-audio offered"
    exit 1
fi
if sed 's/\x1b\[[0-9;]*m//g' "$work/ryll-no-audio.log" \
    | grep 'channel: playback (type=5\|playback: connected successfully' >/dev/null; then
    say "FAIL: andris --no-audio offered ryll a playback channel, or ryll linked one"
    exit 1
fi
say "andris --no-audio offered ryll no playback channel, and ryll linked none"
stop ryll-no-audio INT
stop andris-no-audio TERM

# 12. Stop the rest but Xvfb and the sound server. Andris is stopped as
# systemd stops a unit, with SIGTERM.
stop andris TERM
stop keylogger TERM any

# 13. Neither andris may have logged an error. Colour is off, but strip
# any escape codes anyway so the match cannot be split by one. With the
# sound server up, the first must also have captured sound in every
# session: andris warns each time a client links the playback channel
# and it cannot.
for name in andris andris-no-audio; do
    if sed 's/\x1b\[[0-9;]*m//g' "$work/$name.log" | grep ' ERROR ' >/dev/null; then
        say "FAIL: $name logged an ERROR"
        exit 1
    fi
done
if sed 's/\x1b\[[0-9;]*m//g' "$work/andris.log" \
    | grep 'audio capture could not start\|cannot capture sound' >/dev/null; then
    say "FAIL: andris could not capture sound"
    exit 1
fi

# 14. Ending a session, which stopping each ryll did, logs the metrics.
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

# 15. The desktop going away: another andris on the same display, and
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

# 16. The sound server, last, which exits 0 on SIGTERM.
stop pulse TERM

passed=true
say "PASS"
