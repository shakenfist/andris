#!/bin/bash
# Tag and push a release of andris: the second of a release's two
# stages, run after the release-X.Y.Z pull request that
# scripts/propose-release.sh began has landed on develop.
#
# Fetches origin/develop, checks that its tip carries version X.Y.Z, and,
# after confirmation, creates an annotated tag vX.Y.Z at that commit and
# pushes it. The push starts .github/workflows/release.yml, which builds
# both wheels, waits for approval on the `release` environment, signs
# the tag, creates the GitHub release and publishes the wheels to PyPI.
# A version PyPI has published can never be published again, so this
# step cannot be taken back.
#
# Usage:
#   scripts/tag-release.sh X.Y.Z
#   make tag-release X.Y.Z
#
# Requirements on the host: git, and gh signed in (gh auth login), to
# watch the release workflow. Adapted from ryll's tools/tag-release.sh.

set -euo pipefail

err() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

[[ $# -eq 1 ]] || err "usage: $0 X.Y.Z (e.g. 0.1.0)"
VERSION="$1"
[[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || err "version must be X.Y.Z, got: ${VERSION}"
TAG="v${VERSION}"

command -v git >/dev/null || err "git is not installed"
command -v gh >/dev/null || err "gh is not installed"

cd "$(dirname "$0")/.."
[[ -f Cargo.toml && -f andris/pyproject.toml ]] || err "could not find the repository root"

info "Fetching origin"
git fetch origin develop --tags --quiet

if git rev-parse --verify --quiet "refs/tags/${TAG}" >/dev/null; then
    err "tag ${TAG} already exists locally"
fi
if git ls-remote --tags --exit-code origin "refs/tags/${TAG}" >/dev/null 2>&1; then
    err "tag ${TAG} already exists on origin"
fi

# --- the version on origin/develop ---
#
# Read from the commit that will be tagged, not the working tree, so
# that a local branch or uncommitted edit cannot stand in for what
# landed. Cargo.lock is checked too, since the release builds --frozen.

info "Checking the version on origin/develop"
ACTUAL="$(git show origin/develop:Cargo.toml | scripts/release-version.sh -)" \
    || err "could not read [workspace.package] version from origin/develop"
[[ "${ACTUAL}" == "${VERSION}" ]] \
    || err "origin/develop is at ${ACTUAL}, not ${VERSION}. Has the release-${VERSION} pull request landed?"
git show origin/develop:Cargo.lock \
    | grep -A1 -x 'name = "andris"' | grep -x "version = \"${VERSION}\"" >/dev/null \
    || err "origin/develop's Cargo.lock does not record andris ${VERSION}"

TARGET_SHA="$(git rev-parse origin/develop)"
TARGET_SUBJECT="$(git log -1 --format=%s origin/develop)"

# --- confirm ---

echo
echo "About to tag ${TAG} at ${TARGET_SHA} on origin/develop:"
echo "  ${TARGET_SUBJECT}"
echo
echo "Pushing the tag starts the release workflow, which:"
echo "  - builds the x86_64 and aarch64 wheels and smoke-tests them;"
echo "  - waits for approval on the release environment, then signs the"
echo "    tag and creates the GitHub release;"
echo "  - waits for approval again, then publishes the wheels to PyPI."
echo "    PyPI never accepts a version twice: this cannot be undone."
echo
read -rp "Create and push tag ${TAG}? [y/N] " REPLY
[[ "${REPLY}" =~ ^[Yy]$ ]] || {
    info "Aborted."
    exit 1
}

info "Creating annotated tag ${TAG}"
git tag -a "${TAG}" -m "Release ${VERSION}" "${TARGET_SHA}"

info "Pushing ${TAG} (this starts the release workflow)"
git push origin "refs/tags/${TAG}"

# --- watch ---
#
# The run for this tag is the one whose head is the tag and whose event
# is the push. GitHub can take a few seconds to create it. Failing to
# find or watch it is not a failure of the release, which is already
# under way, so it only says where to look.

info "Waiting for the release workflow to start"
RUN_ID=''
for _ in 1 2 3 4 5 6; do
    sleep 5
    RUN_ID="$(gh run list --workflow=release.yml --branch "${TAG}" --event push \
        --limit 1 --json databaseId --jq '.[0].databaseId // empty' 2>/dev/null || true)"
    [[ -z "${RUN_ID}" ]] || break
done

if [[ -n "${RUN_ID}" ]]; then
    info "Watching workflow run ${RUN_ID}; approve the release environment when it asks"
    gh run watch "${RUN_ID}" || info "the workflow did not complete cleanly"
    gh release view "${TAG}" --web || true
else
    info "Could not find the workflow run. Check: gh run list --workflow=release.yml"
fi
