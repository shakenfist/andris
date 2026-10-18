#!/bin/bash
# Check that a release tag names the version the checkout carries: tag
# vX.Y.Z needs [workspace.package] version X.Y.Z in the root Cargo.toml,
# and the andris entry in Cargo.lock to agree, since the wheel is built
# --frozen from both. Run by release.yml's check-version job.
#
# Usage: scripts/check-release-version.sh GITHUB_REF
#
# A ref that is not a tag -- a workflow_dispatch on a branch, which
# builds the wheels and publishes nothing -- has no version to compare
# against, so the check says so and passes, after checking that the two
# files agree with each other.

set -euo pipefail

REF="${1:?usage: check-release-version.sh GITHUB_REF}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ACTUAL="$("${REPO_ROOT}/scripts/release-version.sh")"
# The andris package's version line in Cargo.lock: the line after
# `name = "andris"` in its [[package]] entry.
LOCKED="$(awk '
    $0 == "name = \"andris\"" { found = 1; next }
    found && /^version = / {
        gsub(/version = "|"/, "")
        print
        exit
    }
' "${REPO_ROOT}/Cargo.lock")"

echo "Ref: ${REF}"
echo "Workspace version: ${ACTUAL}"
echo "Cargo.lock andris version: ${LOCKED:-<none>}"

if [ "${LOCKED}" != "${ACTUAL}" ]; then
    echo "::error::Cargo.lock has andris ${LOCKED:-<none>} but Cargo.toml has ${ACTUAL}; run make lock"
    exit 1
fi

case "${REF}" in
    refs/tags/v*) ;;
    *)
        echo "Not a release tag, so there is no version to compare; building ${ACTUAL} without publishing"
        exit 0
        ;;
esac

TAG="${REF#refs/tags/}"
EXPECTED="${TAG#v}"
echo "Tag expects: ${EXPECTED}"
if [ "${ACTUAL}" != "${EXPECTED}" ]; then
    echo "::error::Version mismatch: tag ${TAG} expects ${EXPECTED} but Cargo.toml's [workspace.package] has ${ACTUAL}"
    exit 1
fi
echo "Tag ${TAG} matches the workspace version"
