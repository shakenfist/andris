#!/bin/bash
# The kerbside lane: andris's end-to-end checks with ryll connected through
# kerbside's proxy, so that every channel andris serves, the playback
# channel among them, crosses kerbside's message allowlist. `make
# e2e-kerbside` runs this from the host once the debug andris and ryll's
# headless client are built and kerbside's repository has been fetched at
# the pinned commit.
#
# It brings up kerbside's demo stack (demo/docker-compose.yml: MariaDB, and
# the released kerbside from PyPI, at the version the Makefile pins) with
# scripts/kerbside-lane/compose.override.yml on top, which swaps the demo's
# qemu for a placeholder, mounts a sources.yaml naming andris at
# 127.0.0.1:5910 with a ticket generated for this run, puts the stack on an
# internal network and publishes no port. Then:
#
#   1. it builds the images with the network up, and brings the stack up
#      on its internal network, waiting for kerbside's health check;
#   2. it mints a .vv with the demo's get-console.sh, which does its work
#      inside the kerbside container: a token, the console, and a check of
#      the TLS leg;
#   3. it runs scripts/e2e.sh with E2E_VIA=kerbside in the andris-dev image,
#      in the kerbside container's network namespace, so andris listens on
#      that namespace's loopback and ryll reaches kerbside's TLS port there;
#   4. it checks kerbside's log: "hypervisor connection successful" for the
#      main, display, inputs, cursor and playback channels, and no firewall
#      verdict, denial, failed backend connection or termination.
#
# On exit, pass or fail, it collects the compose logs and e2e.sh's logs
# into ARTIFACTS, redacts the secrets in them (the .vv's token, the lane's
# ticket, and MariaDB's generated root password), and removes the stack
# with `docker compose down -v`. A secret that survives redaction removes
# the artifacts and fails the run, so they are never uploaded.
#
# Kerbside's tokens are not single use: one is good until it expires, five
# minutes after it is minted (the demo's console_token_duration). But the
# .vv sets delete-this-file=1, so ryll removes it once read. This lane
# starts one ryll, so it mints one .vv.
#
# If kerbside ever needs to reach beyond the stack at run time, the stack
# will not come up on the internal network. Do not open the network to fix
# that: find what needed it, and decide with the maintainers.
#
# Environment:
#   RYLL              the ryll binary (required)
#   ANDRIS            the andris binary, in the checkout's target/ (required)
#   KERBSIDE_DIR      kerbside's repository at the pinned commit (required)
#   KERBSIDE_VERSION  the kerbside release on PyPI that commit is tagged as
#                     (required)
#   ANDRIS_IMAGE      the devcontainer image (default andris-dev)
#   ARTIFACTS         where the redacted logs go (default
#                     target/kerbside-lane); emptied first
#   COMPOSE_PROJECT   the compose project name (default andris-kerbside-lane)

set -euo pipefail

RYLL=${RYLL:?RYLL must name the ryll binary; run this through make e2e-kerbside}
ANDRIS=${ANDRIS:?ANDRIS must name the andris binary; run this through make e2e-kerbside}
KERBSIDE_DIR=${KERBSIDE_DIR:?KERBSIDE_DIR must name kerbside at the pinned commit; run this through make e2e-kerbside}
KERBSIDE_VERSION=${KERBSIDE_VERSION:?KERBSIDE_VERSION must name the pinned release; run this through make e2e-kerbside}
ANDRIS_IMAGE=${ANDRIS_IMAGE:-andris-dev}
COMPOSE_PROJECT=${COMPOSE_PROJECT:-andris-kerbside-lane}
# Where andris listens, in kerbside's namespace.
PORT=5910
# The channels andris serves, each of which kerbside must have relayed.
CHANNELS=(main display inputs cursor playback)
# Bounds, in seconds: on the stack becoming healthy, which includes the
# database initialising, kerbside's `db upgrade` and its TLS generation
# (the health check's own start_period is 120 s), and on the test container.
UP_BOUND=300
TEST_BOUND=600

# Nothing here talks to an X server or a sound server, and nothing is
# passed into a container unless named below. Drop the caller's display
# and sound server anyway, so no later change can reach the desktop this
# was run from.
unset DISPLAY XAUTHORITY PULSE_SERVER PULSE_COOKIE

here=$(cd "$(dirname "$0")" && pwd)
checkout=$(dirname "$here")
artifacts=${ARTIFACTS:-$checkout/target/kerbside-lane}
demo=$(realpath "$KERBSIDE_DIR/demo")
tester=$COMPOSE_PROJECT-e2e-$$

say() {
    echo "[e2e-kerbside] $*"
}

# Docker Compose, before anything slow. The override resets the demo's
# settings with `!reset`, which Compose has understood since 2.24; an
# older one fails later, with an error that does not say why. `version
# --short` prints, say, 2.26.1, v2.24.0 or 2.26.1+ds1; any later major
# version, such as 5.6.0, passes.
COMPOSE_MIN=2.24
compose_install="install Debian's docker-compose package (Debian 13 ships 2.26) or Docker's docker-compose-plugin"
compose_install+=" from download.docker.com"
if ! compose_version=$(docker compose version --short 2>/dev/null); then
    say "FAIL: Docker Compose is missing: \`docker compose\` does not run." \
        "To get $COMPOSE_MIN or newer, $compose_install"
    exit 1
fi
compose_version=${compose_version#v}
compose_major=$(sed -nE 's/^([0-9]+)\.([0-9]+).*/\1/p' <<<"$compose_version")
compose_minor=$(sed -nE 's/^([0-9]+)\.([0-9]+).*/\2/p' <<<"$compose_version")
if [ -z "$compose_major" ] || [ "$compose_major" -lt "${COMPOSE_MIN%.*}" ] \
    || { [ "$compose_major" -eq "${COMPOSE_MIN%.*}" ] && [ "$compose_minor" -lt "${COMPOSE_MIN#*.}" ]; }; then
    say "FAIL: Docker Compose is '$compose_version', and this needs $COMPOSE_MIN or newer, for !reset." \
        "To upgrade, $compose_install"
    exit 1
fi
say "Docker Compose $compose_version"

for binary in "$ANDRIS" "$RYLL"; do
    if [ ! -x "$binary" ]; then
        say "FAIL: $binary is not an executable; run this through make e2e-kerbside"
        exit 1
    fi
done
andris=$(realpath "$ANDRIS")
ryll=$(realpath "$RYLL")
case "$andris" in
    "$checkout"/target/*) ;;
    *)
        say "FAIL: $andris is not under $checkout/target, which is all the test container mounts"
        exit 1
        ;;
esac
if [ ! -f "$demo/docker-compose.yml" ]; then
    say "FAIL: $demo/docker-compose.yml is missing; run this through make e2e-kerbside"
    exit 1
fi

# The run's secrets (the ticket, the sources.yaml holding it, and the .vv)
# live here, 0700, and go when the run ends. The test container mounts it,
# since ryll removes the .vv once read.
run=$(mktemp -d "${TMPDIR:-/tmp}/andris-kerbside-lane.XXXXXX")
rm -rf "$artifacts"
mkdir -p "$artifacts/logs" "$artifacts/e2e"

# Every `docker compose` here and in get-console.sh works on this project:
# the demo's file, then the lane's override. The project name overrides
# the demo's `name: kerbside-demo`, so the lane cannot touch a demo stack
# someone has running. KERBSIDE_SOURCE is what the demo's image installs,
# the pinned release rather than the newest.
export COMPOSE_PROJECT_NAME=$COMPOSE_PROJECT
export COMPOSE_FILE=$demo/docker-compose.yml:$here/kerbside-lane/compose.override.yml
export KERBSIDE_LANE_SOURCES=$run/sources.yaml
export KERBSIDE_SOURCE=kerbside==$KERBSIDE_VERSION

compose() {
    docker compose --project-directory "$demo" "$@"
}

token=
ticket=
passed=false

# Redact the run's secrets from every artifact, after the logs are
# collected; then check that none is left. Values are redacted wherever
# they appear, the token and the ticket by value; the patterns are those of
# kerbside's tools/demo/redact-artifacts.sh, for a .vv's password= line and
# MariaDB's generated root password, which it prints to its log. A file is
# named, never the line that matched, which would copy the secret into the
# job's log.
redact() {
    local secret f left=0
    for secret in "$token" "$ticket"; do
        [ -n "$secret" ] || continue
        while IFS= read -r -d '' f; do
            sed -i "s/$secret/REDACTED/g" "$f"
        done < <(grep -rlZF -- "$secret" "$artifacts" 2>/dev/null || true)
    done
    while IFS= read -r -d '' f; do
        sed -i -e 's/^password=.*/password=REDACTED/' \
            -e 's/GENERATED ROOT PASSWORD: .*/GENERATED ROOT PASSWORD: REDACTED/' "$f"
    done < <(grep -rlZE -- '^password=|GENERATED ROOT PASSWORD: ' "$artifacts" 2>/dev/null || true)
    for secret in "$token" "$ticket"; do
        if [ -n "$secret" ] && grep -rqF -- "$secret" "$artifacts"; then
            left=1
        fi
    done
    if grep -rE -- '^password=|GENERATED ROOT PASSWORD: ' "$artifacts" | grep -qv REDACTED; then
        left=1
    fi
    if [ "$left" -ne 0 ]; then
        say "FAIL: a secret survived redaction; removing the artifacts rather than keeping them"
        rm -rf "$artifacts"
        return 1
    fi
    say "artifacts, redacted, in $artifacts"
}

cleanup() {
    local rc=$? service
    trap - EXIT
    docker kill "$tester" >/dev/null 2>&1 || true
    for service in db spice-target kerbside; do
        compose logs --no-color "$service" >"$artifacts/logs/$service.log" 2>&1 || true
    done
    compose ps --all >"$artifacts/logs/ps.txt" 2>&1 || true
    if ! redact; then
        rc=1
    fi
    say "removing the stack"
    compose down -v --remove-orphans >/dev/null 2>&1 || say "WARNING: docker compose down -v failed"
    rm -rf "$run"
    if [ "$rc" -eq 0 ] && $passed; then
        say "PASS"
    else
        say "FAILED"
        [ "$rc" -ne 0 ] || rc=1
    fi
    exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

say "kerbside $KERBSIDE_VERSION, demo from $KERBSIDE_DIR"

# A clean slate: a stack left by an earlier run that was killed would hold
# its database, CA and signing seed.
compose down -v --remove-orphans >/dev/null 2>&1 || true

# 1. The ticket kerbside presents to andris, 0600 as andris requires, and
# the sources.yaml that holds it. One static console, as the demo's, but
# at andris on the namespace's loopback.
ticket=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
(umask 077 && printf '%s\n' "$ticket" >"$run/ticket")
(umask 077 && cat >"$run/sources.yaml" <<EOF
# Written by andris's scripts/e2e-kerbside.sh for one run.
- source: andris-lane
  type: static
  consoles:
    - uuid: "0d3f6a52-0000-0000-0000-00000a4d7215"
      name: "andris"
      hypervisor: "andris"
      hypervisor_ip: "127.0.0.1"
      insecure_port: $PORT
      ticket: "$ticket"
EOF
)
# Kerbside runs as root in its container, so it can read the 0600 file.

compose config --quiet

# 2. Build with the network up, then start on the internal network.
say "building the demo's images"
compose build --quiet
say "starting the stack on its internal network"
if ! timeout "$UP_BOUND" docker compose --project-directory "$demo" up -d --no-build --wait; then
    say "FAIL: the stack did not become healthy within ${UP_BOUND}s"
    compose ps --all || true
    compose logs --no-color kerbside 2>&1 | tail -n 50 || true
    exit 1
fi
# What was installed, which must be the pinned release, and the proxy it
# pins.
installed=$(compose exec -T kerbside python3 -c '
from importlib.metadata import version
print(version("kerbside"), version("kerbside-proxy"))')
read -r kerbside_installed proxy_installed <<<"$installed"
if [ "$kerbside_installed" != "$KERBSIDE_VERSION" ]; then
    say "FAIL: the stack runs kerbside $kerbside_installed, not the pinned $KERBSIDE_VERSION"
    exit 1
fi
say "the stack is up: kerbside $kerbside_installed, kerbside-proxy $proxy_installed"

# The network is what the override says: no route out of the namespace
# the test container is about to join. A connection to an address beyond
# it must fail at once for want of a route, not by timing out, which a
# firewall would also give.
if ! compose exec -T kerbside python3 -c '
import errno, socket, sys
try:
    socket.create_connection(("192.0.2.1", 443), timeout=5).close()
except OSError as e:
    sys.exit(0 if e.errno == errno.ENETUNREACH else "connecting out failed, but not for want of a route: %s" % e)
sys.exit("connected out of the stack")'; then
    say "FAIL: kerbside's namespace has a route out; the stack is not on an internal network"
    exit 1
fi
say "the stack has no route out"

# 3. The .vv, minted by the demo's own script.
say "minting a .vv with the demo's get-console.sh"
"$demo/get-console.sh" "$run/console.vv"
token=$(sed -n 's/^password=//p' "$run/console.vv" | tr -d '\r')
if [ -z "$token" ]; then
    say "FAIL: the .vv kerbside minted carries no token"
    exit 1
fi
chmod 0600 "$run/console.vv"

# 4. The test container, in kerbside's network namespace. As `make e2e`
# runs it: the checkout read-only with target/ writable, and ryll
# read-only; and the run's directory, for the ticket and the .vv, and the
# artifacts' e2e/, for e2e.sh's logs. It runs in the background and is
# waited for, so that a signal to this script still reaches the trap.
kerbside_id=$(compose ps -q kerbside)
say "running scripts/e2e.sh through kerbside"
rc=0
timeout "$TEST_BOUND" docker run --rm --name "$tester" \
    --network "container:$kerbside_id" \
    -w /workspace \
    -u "$(id -u):$(id -g)" \
    -e HOME=/build \
    -v "$checkout":/workspace:ro \
    -v "$checkout/target":/workspace/target \
    -v "$(dirname "$ryll")":/build/ryll-bin:ro \
    -v "$run":/lane \
    -v "$artifacts/e2e":/lane-logs \
    -e RYLL="/build/ryll-bin/$(basename "$ryll")" \
    -e ANDRIS="/workspace/${andris#"$checkout"/}" \
    -e PORT="$PORT" \
    -e E2E_VIA=kerbside \
    -e E2E_VV=/lane/console.vv \
    -e E2E_TICKET=/lane/ticket \
    -e E2E_LOGS=/lane-logs \
    -e no_proxy=127.0.0.1,localhost \
    -e NO_PROXY=127.0.0.1,localhost \
    "$ANDRIS_IMAGE" \
    scripts/e2e.sh &
wait $! || rc=$?
if [ "$rc" -ne 0 ]; then
    say "FAIL: scripts/e2e.sh exited $rc through kerbside"
fi

# 5. Kerbside's log, whatever the test's result, since a verdict there is
# the likeliest reason for a failure. The strings are kerbside's own, at
# the pinned commit:
#   rust/kerbside-proxy/src/backend.rs  "hypervisor connection successful;
#       handing off to relay", with channel_type= the channel's name, once
#       per channel relayed; and "hypervisor connection failed"
#   rust/kerbside-proxy/src/relay.rs    "policy verdict Terminate; ending
#       relay", "policy header check verdict Terminate; ending relay",
#       "policy verdict Drop; message swallowed" (at debug, so absent
#       unless kerbside logs verbosely), "session terminated by control
#       plane; ending relay", "relay idle-read timeout; ending relay"
#   rust/kerbside-proxy/src/policy.rs   "firewall WarnOnly: would have
#       blocked; forwarding", and the coalesced "Firewall verdicts this
#       connection: ..." that the relay records when anything was tallied,
#       an unmodeled message type among them, which the daemon logs as
#       "Audit: Firewall verdicts this connection: ..." (kerbside/db.py)
#   rust/kerbside-proxy/src/session.rs  "channel type not permitted by
#       firewall policy; denying", "connection denied by control service"
# The first is the one kerbside's own lane asserts, in
# tools/demo/lane-assert.sh, and the one andris's first manual run through
# kerbside recorded for every channel.
log=$(compose logs --no-color kerbside 2>&1 || true)
verdicts=(
    'hypervisor connection failed'
    'policy verdict Terminate; ending relay'
    'policy header check verdict Terminate; ending relay'
    'policy verdict Drop; message swallowed'
    'session terminated by control plane; ending relay'
    'relay idle-read timeout; ending relay'
    'firewall WarnOnly: would have blocked; forwarding'
    'Firewall verdicts this connection'
    'channel type not permitted by firewall policy; denying'
    'connection denied by control service'
)
log_ok=true
for verdict in "${verdicts[@]}"; do
    lines=$(grep -F -- "$verdict" <<<"$log" || true)
    if [ -n "$lines" ]; then
        say "FAIL: kerbside logged \"$verdict\":"
        echo "$lines"
        log_ok=false
    fi
done
for channel in "${CHANNELS[@]}"; do
    if ! grep -F 'hypervisor connection successful; handing off to relay' <<<"$log" \
        | grep -E "channel_type=\"?$channel\"?( |\$)" >/dev/null; then
        say "FAIL: kerbside never logged \"hypervisor connection successful\" for the $channel channel"
        log_ok=false
    fi
done
if $log_ok; then
    say "kerbside relayed the ${CHANNELS[*]} channels, and logged no firewall verdict, denial or termination"
fi

if [ "$rc" -eq 0 ] && $log_ok; then
    passed=true
fi
