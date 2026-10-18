#!/bin/bash
# Print the version in the [workspace.package] table of a Cargo.toml,
# which is andris's one version: the crate inherits it
# (version.workspace = true), and the wheel takes it from the crate.
#
# Usage: scripts/release-version.sh [CARGO_TOML]
#
# CARGO_TOML defaults to the root Cargo.toml; pass - to read standard
# input, as scripts/tag-release.sh does with `git show`. Reads the file
# with awk rather than cargo, so that it runs on a host without a Rust
# toolchain and on a runner without one. Fails if the table or its
# version is missing.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_TOML="${1:-${REPO_ROOT}/Cargo.toml}"

VERSION="$(awk '
    /^\[workspace\.package\]/ { in_wp = 1; next }
    /^\[/ { in_wp = 0 }
    in_wp && /^version *= */ {
        gsub(/version *= *"|"/, "")
        print
        exit
    }
' "${CARGO_TOML}")"

if [ -z "${VERSION}" ]; then
    echo "no [workspace.package] version in ${CARGO_TOML}" >&2
    exit 1
fi
echo "${VERSION}"
