#!/bin/bash
# Build a manylinux_2_28 wheel of andris for one architecture, in an
# image built on the matching manylinux image (.devcontainer/wheel/
# Dockerfile). The wheel embeds the compiled andris binary (maturin's
# bindings = "bin"), which pip installs onto PATH. See
# scripts/build-andris-wheel-in-container.sh for the build and its checks.
#
# Usage: scripts/build-andris-wheel.sh <x86_64|aarch64>
# Output: target/wheels/andris-<version>-py3-none-manylinux_2_28_<arch>.whl
#
# The host must be of the same architecture as <arch>: the build runs a
# native container, not an emulated one.
#
# The build is split as the Makefile splits every compile (see its
# comment on DOCKER_RUN_OFFLINE). `cargo fetch` downloads the crates in
# Cargo.lock with the network up, but compiles nothing, so no
# dependency's build script runs then. The compile then runs with
# --network none, the cargo cache read-only, and the checkout read-only
# apart from target/. The cache is the Makefile's (.cargo-cache, or
# $CARGO_CACHE), so a checkout that has run `make fetch` downloads only
# what is missing. Cargo's own output for the wheel stays in
# target/wheel/<arch>/, apart from the devcontainer's builds, so a second
# build is incremental; treat it as no more trustworthy than the code
# compiled into it, as with the rest of target/.

set -euo pipefail

ARCH="${1:?usage: build-andris-wheel.sh <x86_64|aarch64>}"
case "${ARCH}" in
    x86_64 | aarch64) ;;
    *)
        echo "unsupported architecture: ${ARCH} (expected x86_64 or aarch64)" >&2
        exit 1
        ;;
esac
if [ "$(uname -m)" != "${ARCH}" ]; then
    echo "cannot build an ${ARCH} wheel on $(uname -m): the build runs natively" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="andris-wheel-${ARCH}"
CACHE_DIR="$(cd "${REPO_ROOT}" && realpath -m "${CARGO_CACHE:-.cargo-cache}")"

# target/ is mounted writable over the read-only checkout. It is
# gitignored only as a directory, so a branch could commit a `target`
# symlink, and mounting that writable would hand a build script whatever
# it points at; refuse it, as the Makefile's target-dir does.
if [ -L "${REPO_ROOT}/target" ]; then
    echo "target is a symlink; refusing to mount it writable" >&2
    exit 1
fi

docker build -t "${IMAGE}" --build-arg "ARCH=${ARCH}" \
    "${REPO_ROOT}/.devcontainer/wheel"

# Created here, as the invoking user, so that docker does not create them
# owned by root. A wheel left by an earlier build is removed first, so a
# failed build leaves none behind to be mistaken for its output.
mkdir -p "${CACHE_DIR}/registry" "${CACHE_DIR}/git" \
    "${REPO_ROOT}/target/wheels" "${REPO_ROOT}/target/wheel/${ARCH}"
rm -f "${REPO_ROOT}/target/wheels/"andris-*_"${ARCH}".whl

RUN_ARGS=(
    --rm
    -v "${REPO_ROOT}:/work:ro"
    -w /work
    -u "$(id -u):$(id -g)"
    -e HOME=/build
    -e CARGO_BUILD_JOBS
)

docker run "${RUN_ARGS[@]}" \
    -v "${CACHE_DIR}/registry:/build/.cargo/registry" \
    -v "${CACHE_DIR}/git:/build/.cargo/git" \
    "${IMAGE}" \
    cargo fetch --locked

docker run "${RUN_ARGS[@]}" \
    --network none \
    -v "${REPO_ROOT}/target:/work/target" \
    -v "${CACHE_DIR}/registry:/build/.cargo/registry:ro" \
    -v "${CACHE_DIR}/git:/build/.cargo/git:ro" \
    -e "WHEEL_ARCH=${ARCH}" \
    "${IMAGE}" \
    /work/scripts/build-andris-wheel-in-container.sh

echo 'Built wheel:'
ls -l "${REPO_ROOT}/target/wheels/"andris-*_"${ARCH}".whl
