#!/bin/bash
# Tag and push a release of andris: the second of a release's two
# stages, run after the release-X.Y.Z pull request that
# scripts/propose-release.sh began has landed on develop.
#
# Fetches origin/develop, finds the commit that landed the release
# (the merge of the pull request holding "Release X.Y.Z.", or that commit
# itself if it landed directly), checks that it carries version X.Y.Z,
# and, after confirmation, creates an annotated tag vX.Y.Z at that commit
# and pushes it. Anything that landed on develop after it is listed, and
# is not in the release. The push starts .github/workflows/release.yml, which builds
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

# --- the commit that landed the release ---
#
# Not develop's tip: another pull request may have landed since, and
# what is tagged is what is built and published, so it must be what was
# reviewed as the release. The release commit is the one
# propose-release.sh made; the commit that landed it is the first commit
# on develop's first-parent line to contain it.

info "Finding the commit that landed release ${VERSION}"
RELEASE_SHA="$(git log origin/develop --format='%H%x09%s' \
    | awk -F '\t' -v want="Release ${VERSION}." '$2 == want { print $1; exit }')"
[[ -n "${RELEASE_SHA}" ]] \
    || err "no commit 'Release ${VERSION}.' on origin/develop. Has the release-${VERSION} pull request landed?"
TARGET_SHA=''
while read -r commit; do
    if git merge-base --is-ancestor "${RELEASE_SHA}" "${commit}"; then
        TARGET_SHA="${commit}"
        break
    fi
done < <(git rev-list --first-parent --reverse origin/develop --not "${RELEASE_SHA}^@")
[[ -n "${TARGET_SHA}" ]] || err "could not find where ${RELEASE_SHA} landed on origin/develop"
TARGET_SUBJECT="$(git log -1 --format=%s "${TARGET_SHA}")"

# --- the version there ---
#
# Read from the commit that will be tagged, not the working tree, so
# that a local branch or uncommitted edit cannot stand in for what
# landed. Cargo.lock is checked too, since the release builds --frozen.

info "Checking the version at ${TARGET_SHA}"
ACTUAL="$(git show "${TARGET_SHA}:Cargo.toml" | scripts/release-version.sh -)" \
    || err "could not read [workspace.package] version at ${TARGET_SHA}"
[[ "${ACTUAL}" == "${VERSION}" ]] \
    || err "${TARGET_SHA} is at ${ACTUAL}, not ${VERSION}"
git show "${TARGET_SHA}:Cargo.lock" \
    | grep -A1 -x 'name = "andris"' | grep -x "version = \"${VERSION}\"" >/dev/null \
    || err "the Cargo.lock at ${TARGET_SHA} does not record andris ${VERSION}"

LATER="$(git log --oneline "${TARGET_SHA}..origin/develop")"

# --- confirm ---

echo
echo "About to tag ${TAG} at ${TARGET_SHA}, which landed the release:"
echo "  ${TARGET_SUBJECT}"
if [[ -n "${LATER}" ]]; then
    echo
    echo "These landed on origin/develop after it, and are NOT in ${TAG}:"
    while IFS= read -r line; do echo "  ${line}"; done <<< "${LATER}"
fi
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
