#!/bin/bash
# Runs inside the andris-wheel-smoke container (python:3-slim, with no
# network), as scripts/wheel-smoke.sh starts it; it is not meant to be run
# on a normal host. It installs the wheel named by $WHEEL into a virtual
# environment, as pipx does, and checks that:
#
# - `andris --version` names the wheel's version;
# - `andris install-unit` writes a unit whose ExecStart names the
#   installed binary, and otherwise matches the template at $TEMPLATE, and
#   a private ticket;
# - `andris --platform synthetic` starts, says it is listening, and exits
#   0 on SIGTERM;
# - `andris --platform x11`, with no libpulse here, warns that it serves
#   without sound, rather than failing at exec, and then fails to open the
#   display (there is no X server) with andris's own exit status for a
#   runtime failure, 1, not a crash.
#
# Every log is printed when a check fails.

set -euo pipefail

: "${WHEEL:?WHEEL must name the wheel}"
: "${TEMPLATE:?TEMPLATE must name the unit template}"

WORK="$(mktemp -d)"
mkdir -p "${HOME}"

fail() {
    echo "FAIL: $*" >&2
    for log in "${WORK}"/*.log; do
        [ -f "${log}" ] || continue
        echo "--- ${log}" >&2
        cat "${log}" >&2
    done
    exit 1
}

pass() {
    echo "ok: $*"
}

# The version is the wheel's, from its file name:
# andris-<version>-py3-none-<platform>.whl.
WHEEL_NAME="$(basename "${WHEEL}")"
VERSION="${WHEEL_NAME#andris-}"
VERSION="${VERSION%%-*}"

# --no-index: there is no network, and the wheel has no dependencies. pip
# also refuses a wheel whose platform tag this interpreter cannot run,
# which checks the tag against a stock glibc host.
python3 -m venv "${WORK}/venv"
"${WORK}/venv/bin/pip" install --quiet --no-index --no-deps --disable-pip-version-check "${WHEEL}" \
    > "${WORK}/pip.log" 2>&1 || fail "pip could not install ${WHEEL_NAME}"
ANDRIS="${WORK}/venv/bin/andris"
[ -x "${ANDRIS}" ] || fail "the wheel installed no andris at ${ANDRIS}"
pass "pip installed ${WHEEL_NAME}"

# A check is only worth something if libpulse really is missing here.
if ldconfig -p | grep -q libpulse; then
    fail 'this image has libpulse, so the no-libpulse check would prove nothing'
fi

# --version
OUTPUT="$("${ANDRIS}" --version 2>&1)" || fail "andris --version failed: ${OUTPUT}"
[ "${OUTPUT}" = "andris ${VERSION}" ] || fail "andris --version printed '${OUTPUT}', not 'andris ${VERSION}'"
pass "andris --version prints '${OUTPUT}'"

# install-unit, into a scratch XDG_CONFIG_HOME. The unit must be the
# template with only its ExecStart= changed, from the bare andris to the
# installed binary's absolute path, double-quoted.
CONFIG="${WORK}/config"
env -u DISPLAY XDG_CONFIG_HOME="${CONFIG}" "${ANDRIS}" install-unit > "${WORK}/install-unit.log" 2>&1 \
    || fail 'andris install-unit failed'
UNIT="${CONFIG}/systemd/user/andris@.service"
[ -f "${UNIT}" ] || fail "andris install-unit wrote no unit at ${UNIT}"
EXE="$(readlink -f "${ANDRIS}")"
grep -Fqx "ExecStart=\"${EXE}\" --display :%i --ticket-file %E/andris/ticket \$ANDRIS_ARGS" "${UNIT}" \
    || fail "the unit's ExecStart does not name ${EXE}: $(grep '^ExecStart' "${UNIT}" || true)"
EXPECTED="$(sed "s|^ExecStart=andris |ExecStart=\"${EXE}\" |" "${TEMPLATE}")"
[ "$(cat "${UNIT}")" = "${EXPECTED}" ] || fail 'the unit differs from the template beyond its ExecStart'
TICKET_MODE="$(stat -c %a "${CONFIG}/andris/ticket")" || fail 'andris install-unit wrote no ticket'
[ "${TICKET_MODE}" = 600 ] || fail "the ticket has mode ${TICKET_MODE}, not 600"
DIR_MODE="$(stat -c %a "${CONFIG}/andris")"
[ "${DIR_MODE}" = 700 ] || fail "the configuration directory has mode ${DIR_MODE}, not 700"
pass "andris install-unit wrote a unit running ${EXE}, and a private ticket"

# A ticket for the two servers below.
TICKET="${WORK}/ticket"
(umask 077 && head -c 24 /dev/urandom | base64 > "${TICKET}")

# The synthetic platform: it must say it is listening, then exit 0 on
# SIGTERM.
"${ANDRIS}" --platform synthetic --listen 127.0.0.1:5900 --ticket-file "${TICKET}" \
    > "${WORK}/synthetic.log" 2>&1 &
PID=$!
for _ in $(seq 100); do
    grep -q 'plain listener ready' "${WORK}/synthetic.log" && break
    kill -0 "${PID}" 2> /dev/null || break
    sleep 0.1
done
grep -q 'plain listener ready' "${WORK}/synthetic.log" || fail 'the synthetic andris never said it was listening'
kill -TERM "${PID}"
STATUS=0
wait "${PID}" || STATUS=$?
[ "${STATUS}" -eq 0 ] || fail "the synthetic andris exited ${STATUS} on SIGTERM, not 0"
pass 'andris --platform synthetic listened, and exited 0 on SIGTERM'

# The X11 platform with no libpulse and no X server. Sound is resolved
# before the display is opened, so the warning comes first; then opening
# :99 fails, and andris exits 1, as for any runtime failure. A status of 2
# would be a refused configuration, 101 a panic, and above 128 a signal.
STATUS=0
timeout 30 "${ANDRIS}" --platform x11 --display :99 --ticket-file "${TICKET}" \
    > "${WORK}/x11.log" 2>&1 || STATUS=$?
WARNING='cannot load libpulse, so andris serves without sound'
grep -q "${WARNING}" "${WORK}/x11.log" || fail "andris did not warn '${WARNING}'"
grep -q 'exporting X display :99' "${WORK}/x11.log" || fail 'andris never tried to open the display'
WARNED_AT="$(grep -n "${WARNING}" "${WORK}/x11.log" | head -1 | cut -d: -f1)"
OPENED_AT="$(grep -n 'exporting X display :99' "${WORK}/x11.log" | head -1 | cut -d: -f1)"
[ "${WARNED_AT}" -lt "${OPENED_AT}" ] || fail 'andris warned about libpulse only after opening the display'
if grep -q 'panicked' "${WORK}/x11.log"; then
    fail 'andris panicked'
fi
[ "${STATUS}" -eq 1 ] || fail "andris exited ${STATUS} when the display could not be opened, not 1"
pass 'andris --platform x11 warned that it serves without sound, then exited 1 for want of a display'

echo 'The wheel passed every check.'
