#!/bin/bash
# Smoke-test an andris wheel in a stock python:3-slim container, which has
# no libpulse, no X server and no systemd: install it as pipx would, into
# a virtual environment, and check that the binary it puts on PATH runs.
# The checks are scripts/wheel-smoke-in-container.sh's.
#
# Usage: scripts/wheel-smoke.sh [WHEEL]
#
# WHEEL defaults to the one wheel in target/wheels for this host's
# architecture, as `make wheel` leaves it; only a wheel for this host's
# architecture can run here.
#
# The image is built from .devcontainer/wheel-smoke/Dockerfile, which pins
# python:3-slim by digest; building it is the only step with the network.
# The container then runs with --network none, so andris can reach no X
# server and no sound server, nor anything else, and with only the wheel
# and the files the checks read mounted, read-only. It runs as the
# invoking user, and writes only inside itself.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE=andris-wheel-smoke
ARCH="$(uname -m)"

if [ "$#" -gt 1 ]; then
    echo "usage: $0 [WHEEL]" >&2
    exit 2
fi
if [ "$#" -eq 1 ]; then
    WHEEL="$1"
else
    shopt -s nullglob
    WHEELS=("${REPO_ROOT}/target/wheels/"andris-*_"${ARCH}".whl)
    shopt -u nullglob
    if [ "${#WHEELS[@]}" -ne 1 ]; then
        echo "expected one ${ARCH} wheel in target/wheels, found ${#WHEELS[@]}; run make wheel" >&2
        exit 1
    fi
    WHEEL="${WHEELS[0]}"
fi
if [ ! -f "${WHEEL}" ]; then
    echo "no wheel at ${WHEEL}" >&2
    exit 1
fi
WHEEL="$(realpath "${WHEEL}")"
echo "Smoke-testing $(basename "${WHEEL}")"

docker build -t "${IMAGE}" "${REPO_ROOT}/.devcontainer/wheel-smoke"

# The unit template is mounted so that the install-unit check can compare
# the rendered unit with it, line for line. DISPLAY and the PulseAudio
# variables are never passed in: the container's environment is the
# image's, plus what is named here.
docker run --rm \
    --network none \
    -u "$(id -u):$(id -g)" \
    -e HOME=/tmp/home \
    -e "WHEEL=/smoke/wheel/$(basename "${WHEEL}")" \
    -e TEMPLATE=/smoke/andris@.service \
    -v "${WHEEL}:/smoke/wheel/$(basename "${WHEEL}"):ro" \
    -v "${REPO_ROOT}/packaging/systemd/andris@.service:/smoke/andris@.service:ro" \
    -v "${REPO_ROOT}/scripts/wheel-smoke-in-container.sh:/smoke/run.sh:ro" \
    "${IMAGE}" \
    /smoke/run.sh
