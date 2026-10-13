#!/bin/bash
# The xrdp test bed's run, inside the andris-xrdp-testbed container, as
# root. scripts/e2e-xrdp.sh starts the container (`make e2e-xrdp`) with no
# network, the checkout mounted read-only at /workspace, and the andris and
# ryll binaries mounted at ANDRIS and RYLL. It refuses to run anywhere
# else: it starts daemons, creates a user and rewrites xrdp's config.
#
# It builds a real xrdp session and has andris share it:
#   1. creates the user `tester`, whose ~/startwm.sh runs
#      scripts/e2e-keylogger.py as the whole session;
#   2. starts xrdp-sesman, and xrdp listening on loopback;
#   3. starts an Xvfb for the RDP client, and xfreerdp3 on it, which logs
#      in as tester, so that sesman starts an xorgxrdp Xorg and the
#      keylogger on it;
#   4. waits for that Xorg, reading its display number from sesman's log,
#      and for the keylogger's window;
#   5. as tester, starts andris on the session's display, and ryll
#      --headless against andris, as scripts/e2e.sh does.
#
# Then the smoke checks, each printing PASS or FAIL:
#   capture   andris captures the screen with MIT-SHM, since the display
#             is local;
#   surface   ryll's screenshot has the session's size, and a frame of the
#             keylogger's window reached it (scripts/xrdp-testbed/smoke.py);
#   channels  ryll's log shows the main, display, inputs and cursor
#             channels connected.
#
# If they pass, the interleaving checks X1 to X11 follow
# (scripts/e2e-xrdp-drive.py): keys from RDP and SPICE in turn, held
# modifiers, lock keys, an RDP resize, an RDP reconnect with the same and
# with another layout, clipboard text each way, no clipboard loop through
# a stand-in for a shared pasteboard, a SPICE resize refused, and last,
# the session ending. Andris must handle
# that by exiting by itself, promptly and with status 1, the status it
# gives when the desktop goes, rather than hanging, crashing or exiting
# 0.
#
# Sesman numbers sessions from X11DisplayOffset, 10 by default, so a
# session here would be :10, the same number as the operator's real xrdp
# desktop on the host. Nothing here could reach that one (the container
# has its own network namespace, so its own abstract X sockets, and its own
# /tmp), but sharing the number makes every log and command line here
# ambiguous to a reader. So the offset is set to 50, and the display is
# read from sesman's log rather than assumed.
#
# It exits 0 only if every check passed. On failure it prints every log.
#
# Environment:
#   ANDRIS  the andris binary (required)
#   RYLL    the ryll binary, built by `make ryll-headless` (required)

set -euo pipefail

MARKER=/etc/andris-xrdp-testbed
if [ ! -f "$MARKER" ]; then
    echo "[e2e-xrdp] refusing to run: no $MARKER, so this is not the andris-xrdp-testbed container" >&2
    exit 2
fi

ANDRIS=${ANDRIS:?ANDRIS must name the andris binary; run this through make e2e-xrdp}
RYLL=${RYLL:?RYLL must name the ryll binary; run this through make e2e-xrdp}

# The session's user. This is a throwaway container on --network none, and
# xrdp listens on its loopback only, so the password is not a secret: it
# exists because xrdp will not log in a user without one.
TESTER=tester
TESTER_PASSWORD=andris-testbed-not-a-secret
TESTER_HOME=/home/$TESTER

# Sesman's first session display; see the header.
DISPLAY_OFFSET=50
# The RDP client's own X server, and the size it asks the session to be.
CLIENT_DISPLAY=:1
CLIENT_GEOMETRY=1600x1000
RDP_SIZE=1280x800
RDP_PORT=3389
SPICE_PORT=5930

# The versions the operator's host runs; a difference is reported, not
# failed, since Debian updates both.
HOST_XRDP=0.10.1-3.1+deb13u2
HOST_XORGXRDP=1:0.10.2-1

SESMAN_LOG=/var/log/xrdp-sesman.log
XRDP_LOG=/var/log/xrdp.log

# Bounds, in seconds, on each wait.
DAEMON_BOUND=10
XVFB_BOUND=10
SESSION_BOUND=30
KEYLOGGER_BOUND=15
ANDRIS_BOUND=10

here=$(cd "$(dirname "$0")" && pwd)
scripts=$(dirname "$here")
work=$(mktemp -d /tmp/e2e-xrdp.XXXXXX)
sock=$work/ryll.sock

declare -A pids=()
names=()
failures=0
passed=false

say() {
    echo "[e2e-xrdp] $*"
}

pass() {
    say "PASS: $*"
}

fail() {
    say "FAIL: $*"
    failures=$((failures + 1))
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

# A command prefix that runs the rest of its command line as tester, in
# tester's environment for the session's display. setpriv and env each
# exec what follows, so a process started through it keeps its own pid,
# and signals reach it directly.
as_tester=(setpriv --reuid="$TESTER" --regid="$TESTER" --init-groups --reset-env
    env HOME="$TESTER_HOME" USER="$TESTER" XAUTHORITY="$TESTER_HOME/.Xauthority" NO_COLOR=1)

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

# The log checks send grep's output to /dev/null rather than using -q, as
# scripts/e2e.sh explains: grep -q can kill sed with SIGPIPE, which
# pipefail turns into a failure.
plain() {
    sed 's/\x1b\[[0-9;]*m//g' "$1"
}

cleanup() {
    local name log
    for name in "${names[@]}"; do
        if alive "${pids[$name]}"; then
            kill -KILL "${pids[$name]}" 2>/dev/null || true
        fi
        { wait "${pids[$name]}"; } 2>/dev/null || true
    done
    if ! $passed; then
        # Every process's log, including those the interleaving checks
        # restarted, and the keylogger's.
        for log in "$work"/*.log; do
            if [ -f "$log" ]; then
                echo "===== $(basename "$log") ====="
                cat "$log"
            fi
        done
        # Sesman runs the session's Xorg in tester's home, logging to
        # .xorgxrdp.<display>.log there.
        for log in "$SESMAN_LOG" "$XRDP_LOG" "$TESTER_HOME"/.xorgxrdp.*.log; do
            if [ -f "$log" ]; then
                echo "===== $log ====="
                cat "$log"
            fi
        done
        say "FAILED"
    fi
    rm -rf "$work"
}
trap cleanup EXIT

# Background jobs of a non-interactive shell ignore SIGINT and SIGQUIT.
# Nothing here sends those (the checks stop ryll and FreeRDP with SIGTERM,
# and the cleanup kills with SIGKILL), but job control puts each process in
# its own process group with default signal dispositions, so each runs as
# it would when started from a terminal or by systemd.
set -m

say "$("$RYLL" --version 2>&1 | head -n1 || true); $("$ANDRIS" --version 2>&1 | head -n1 || true)"
xrdp_version=$(dpkg-query -W -f '${Version}' xrdp)
xorgxrdp_version=$(dpkg-query -W -f '${Version}' xorgxrdp)
freerdp_version=$(dpkg-query -W -f '${Version}' freerdp3-x11)
say "xrdp $xrdp_version, xorgxrdp $xorgxrdp_version, freerdp3-x11 $freerdp_version"
if [ "$xrdp_version" != "$HOST_XRDP" ] || [ "$xorgxrdp_version" != "$HOST_XORGXRDP" ]; then
    say "NOTE: the operator's host runs xrdp $HOST_XRDP and xorgxrdp $HOST_XORGXRDP"
fi

# 1. The session's user, and its session: the keylogger and nothing else.
# Sesman runs ~/startwm.sh in place of /etc/xrdp/startwm.sh
# (UserWindowManager in sesman.ini), on the session's display, and ends
# the session when it exits. The keylogger's output goes in the work
# directory, which tester owns, so that this script can wait on it.
useradd --create-home --shell /bin/bash "$TESTER"
echo "$TESTER:$TESTER_PASSWORD" | chpasswd
chown "$TESTER:" "$work"
cat >"$TESTER_HOME/startwm.sh" <<EOF
#!/bin/sh
exec python3 -u $scripts/e2e-keylogger.py --display "\$DISPLAY" --detail >$work/keylogger.log 2>&1
EOF
chown "$TESTER:" "$TESTER_HOME/startwm.sh"
chmod 0755 "$TESTER_HOME/startwm.sh"

# 2. Sesman and xrdp. Sesman's config is Debian's, with the display offset
# moved (see the header). KillDisconnected is false, as it is on the host,
# so a session outlives its RDP connection; it is set explicitly so that a
# change of Debian's default cannot change what the test bed tests.
sed -i -e "s/^X11DisplayOffset=.*/X11DisplayOffset=$DISPLAY_OFFSET/" \
    -e 's/^KillDisconnected=.*/KillDisconnected=false/' /etc/xrdp/sesman.ini
mkdir -p /run/xrdp
chown xrdp:xrdp /run/xrdp
start sesman xrdp-sesman --nodaemon
wait_until "$DAEMON_BOUND" sesman "sesman listening" grep "Sesman now listening" "$SESMAN_LOG"
start xrdp xrdp --nodaemon --port "tcp://127.0.0.1:$RDP_PORT"
wait_until "$DAEMON_BOUND" xrdp "xrdp listening on 127.0.0.1:$RDP_PORT" \
    grep "listening to port $RDP_PORT on 127.0.0.1" "$XRDP_LOG"

# 3. The RDP client, on an Xvfb of its own, larger than the session so the
# whole FreeRDP window fits on it. /dynamic-resolution lets a resize of
# that window resize the session. -grab-keyboard stops FreeRDP grabbing
# the client's keyboard while it has the focus, so that the interleaving
# checks can change the client's own lock keys without FreeRDP seeing them.
# The checks restart FreeRDP with this same command.
start client-xvfb Xvfb "$CLIENT_DISPLAY" -screen 0 "${CLIENT_GEOMETRY}x24" -nolisten tcp
wait_until "$XVFB_BOUND" client-xvfb "the client's Xvfb answering on $CLIENT_DISPLAY" \
    xdpyinfo -display "$CLIENT_DISPLAY"
freerdp_command=(env DISPLAY="$CLIENT_DISPLAY" xfreerdp3 "/v:127.0.0.1:$RDP_PORT" "/u:$TESTER"
    "/p:$TESTER_PASSWORD" /cert:ignore "/size:$RDP_SIZE" /dynamic-resolution -grab-keyboard)
start freerdp "${freerdp_command[@]}"

# 4. The session's Xorg, and the keylogger on it. FreeRDP is the process
# watched: if the RDP connection fails, it exits.
session_display() {
    sed -n 's/.*X server :\([0-9][0-9]*\) is working.*/\1/p' "$SESMAN_LOG" | head -n1
}
session_working() {
    [ -n "$(session_display)" ]
}
wait_until "$SESSION_BOUND" freerdp "sesman's Xorg working" session_working
session=:$(session_display)
say "the xrdp session is on display $session"
wait_until "$KEYLOGGER_BOUND" freerdp "keylogger mapped" grep -qx READY "$work/keylogger.log"
session_size=$("${as_tester[@]}" xdpyinfo -display "$session" |
    sed -n 's/^ *dimensions: *\([0-9]*x[0-9]*\) pixels.*/\1/p')
say "the session is $session_size"

# 5. Andris and ryll, as tester, as scripts/e2e.sh runs them. The ticket
# is 0600 and tester's, as andris requires.
ticket=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
(umask 077 && printf '%s\n' "$ticket" >"$work/ticket")
(umask 077 && printf '[virt-viewer]\ntype=spice\nhost=127.0.0.1\nport=%s\npassword=%s\n' \
    "$SPICE_PORT" "$ticket" >"$work/andris.vv")
chown "$TESTER:" "$work/ticket" "$work/andris.vv"

start andris "${as_tester[@]}" RUST_LOG=info "$ANDRIS" --platform x11 --display "$session" \
    --listen "127.0.0.1:$SPICE_PORT" --ticket-file "$work/ticket"
wait_until "$ANDRIS_BOUND" andris "andris listening on $SPICE_PORT" \
    grep -q "plain listener ready" "$work/andris.log"
ryll_command=("${as_tester[@]}" "$RYLL" --headless --file "$work/andris.vv" --control-socket "$sock" --verbose)
start ryll "${ryll_command[@]}"

# The smoke checks. smoke.py bounds its own waits, and gives up early if a
# process it depends on exits.
rc=0
python3 "$here/smoke.py" --socket "$sock" --size "$session_size" \
    --watch "andris=${pids[andris]}" --watch "ryll=${pids[ryll]}" --watch "freerdp=${pids[freerdp]}" \
    >"$work/smoke.log" 2>&1 || rc=$?
cat "$work/smoke.log"
if [ "$rc" -ne 0 ]; then
    failures=$((failures + 1))
fi

method=$(plain "$work/andris.log" | grep -o 'capturing the screen with [A-Za-z-]*' | head -n1 || true)
if [ "$method" = "capturing the screen with MIT-SHM" ]; then
    pass "andris is $method"
else
    fail "andris is not capturing with MIT-SHM: ${method:-it never said how it captures}"
fi

# Ryll's log is the oracle for the channels, as in scripts/e2e.sh.
missing=()
for channel in main display inputs cursor; do
    if ! plain "$work/ryll.log" | grep " INFO $channel: connected successfully\$" >/dev/null; then
        missing+=("$channel")
    fi
done
if [ "${#missing[@]}" -eq 0 ]; then
    pass "ryll connected the main, display, inputs and cursor channels"
else
    fail "ryll never connected the ${missing[*]} channel(s)"
fi

for name in "${names[@]}"; do
    if ! alive "${pids[$name]}"; then
        fail "$name exited early"
    fi
done

if [ "$failures" -ne 0 ]; then
    say "$failures smoke check(s) failed; not running the interleaving checks"
    exit 1
fi

# The interleaving checks, X1 to X11 (scripts/e2e-xrdp-drive.py). They
# restart ryll and FreeRDP with the commands above, and end the session
# last, in X7. Andris is this script's child, so X7's last part, andris's
# exit status, is checked here once the drive has seen it exit.
rc=0
python3 "$scripts/e2e-xrdp-drive.py" --socket "$sock" --session "$session" --client-display "$CLIENT_DISPLAY" \
    --work "$work" --xauthority "$TESTER_HOME/.Xauthority" --xrdp-log "$XRDP_LOG" \
    --andris "${pids[andris]}" --ryll "${pids[ryll]}" --freerdp "${pids[freerdp]}" \
    --ryll-command "${ryll_command[*]@Q}" --freerdp-command "${freerdp_command[*]@Q}" || rc=$?
if [ "$rc" -ne 0 ]; then
    failures=$((failures + 1))
fi
if alive "${pids[andris]}"; then
    fail "X7 (exit status): andris is still running"
else
    # Exactly 1, as scripts/e2e.sh's fails_within requires: andris
    # returning an error. A crash or a signal gives another status.
    status=0
    wait "${pids[andris]}" || status=$?
    if [ "$status" -eq 1 ]; then
        pass "X7 (exit status): andris exited with status $status"
    else
        fail "X7 (exit status): andris exited with status $status when the desktop went, not 1"
    fi
fi
if [ "$failures" -ne 0 ]; then
    say "$failures interleaving check(s) failed"
    exit 1
fi

passed=true
say "PASS"
