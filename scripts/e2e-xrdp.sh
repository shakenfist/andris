#!/bin/bash
# The xrdp test bed, from the host: andris sharing a real xrdp session, in a
# throwaway container. `make e2e-xrdp` runs this once the release andris and
# ryll's headless client are built. It builds the andris-xrdp-testbed image
# from scripts/xrdp-testbed/Dockerfile when the image is missing or out of
# date (it needs the andris-dev image, which the Makefile builds), then
# runs scripts/xrdp-testbed/run.sh in a container and exits with its
# status.
#
# The container runs with no network, so xrdp, the RDP client, andris and
# ryll talk over its own loopback, and it has its own /tmp and abstract
# socket namespace: no X server on the host, and no xrdp session there, is
# reachable from inside it. The checkout is mounted read-only at
# /workspace, and the two binaries read-only beside it. Nothing is written
# outside the container, which is removed when the run ends.
#
# Docker flags beyond that:
#   --init        a real pid 1, which reaps the session's processes when
#                 sesman's children exit, and passes on Ctrl-C;
#   --shm-size    room in /dev/shm for the X servers and their clients.
#
# Environment:
#   RYLL    the ryll binary, built by `make ryll-headless` (required)
#   ANDRIS  the andris binary (default target/release/andris)

set -euo pipefail

RYLL=${RYLL:?RYLL must name the ryll binary; run this through make e2e-xrdp}
ANDRIS=${ANDRIS:-target/release/andris}
IMAGE='andris-xrdp-testbed'
KEY_LABEL=org.shakenfist.andris.testbed-key

# Nothing here talks to an X server or a sound server, and nothing is
# passed into the container unless named below. Drop the caller's
# display and sound server anyway, so no later change can reach the
# desktop this was run from.
unset DISPLAY XAUTHORITY PULSE_SERVER PULSE_COOKIE

here=$(cd "$(dirname "$0")" && pwd)
checkout=$(dirname "$here")
name=andris-xrdp-testbed-$$

say() {
    echo "[e2e-xrdp] $*"
}

for binary in "$ANDRIS" "$RYLL"; do
    if [ ! -x "$binary" ]; then
        say "FAIL: $binary is not an executable; run this through make e2e-xrdp"
        exit 1
    fi
done
andris=$(realpath "$ANDRIS")
ryll=$(realpath "$RYLL")

# Build the image only when its inputs have changed. Docker's own layer
# cache cannot decide that here: every `make devcontainer` gives andris-dev
# a new image id, though its layers are unchanged, and a new parent id
# reruns this image's apt-get, which takes half a minute and the network.
# So the key is the Dockerfile and the base image's layer digests, and
# the image carries it as a label.
base_layers=$(docker image inspect andris-dev --format '{{json .RootFS.Layers}}')
key=$({ cat "$here/xrdp-testbed/Dockerfile"; echo "$base_layers"; } | sha256sum | cut -d' ' -f1)
built=$(docker image inspect "$IMAGE" --format "{{index .Config.Labels \"$KEY_LABEL\"}}" 2>/dev/null || true)
if [ "$built" = "$key" ]; then
    say "$IMAGE is current"
else
    say "building $IMAGE"
    docker build --quiet --label "$KEY_LABEL=$key" -t "$IMAGE" "$here/xrdp-testbed" >/dev/null
fi

# If this script is interrupted or killed, the container goes too. `docker
# run --rm` removes it once it stops. The container runs in the background
# and is waited for, because bash runs a trap only once the foreground
# command has finished, and a SIGTERM to this script alone would otherwise
# leave the container running to the end.
cleanup() {
    docker kill "$name" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

docker run --rm --name "$name" \
    --network none \
    --init \
    --shm-size 256m \
    -v "$checkout":/workspace:ro \
    -v "$andris":/testbed/andris:ro \
    -v "$ryll":/testbed/ryll:ro \
    -e ANDRIS=/testbed/andris \
    -e RYLL=/testbed/ryll \
    "$IMAGE" \
    /workspace/scripts/xrdp-testbed/run.sh &
# The run's status is this script's: with set -e, a failure exits with it.
wait $!
