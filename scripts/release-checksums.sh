#!/bin/bash
# Check that a directory holds exactly the wheels a release publishes,
# one per architecture, at the release's version, and write their
# SHA256SUMS beside them. Run by release.yml's github-release job on the
# downloaded wheels, before they are attached to the GitHub release.
#
# Usage: scripts/release-checksums.sh DIR VERSION
#
# Anything else in DIR fails the check rather than being published: the
# release's assets are the two wheels and SHA256SUMS, and nothing more.
# The checksums use bare file names, so that `sha256sum -c SHA256SUMS`
# works in whatever directory the assets are downloaded to.

set -euo pipefail

DIR="${1:?usage: release-checksums.sh DIR VERSION}"
VERSION="${2:?usage: release-checksums.sh DIR VERSION}"
ARCHES=(x86_64 aarch64)

cd "${DIR}"

EXPECTED=()
for arch in "${ARCHES[@]}"; do
    wheel="andris-${VERSION}-py3-none-manylinux_2_28_${arch}.whl"
    if [ ! -f "${wheel}" ]; then
        echo "missing ${wheel} in ${DIR}" >&2
        exit 1
    fi
    EXPECTED+=("${wheel}")
done

shopt -s nullglob dotglob
for path in *; do
    found=0
    for wheel in "${EXPECTED[@]}"; do
        if [ "${path}" = "${wheel}" ]; then
            found=1
        fi
    done
    if [ "${found}" -ne 1 ]; then
        echo "unexpected ${path} in ${DIR}; a release publishes only ${EXPECTED[*]}" >&2
        exit 1
    fi
done
shopt -u nullglob dotglob

sha256sum "${EXPECTED[@]}" > SHA256SUMS
cat SHA256SUMS
