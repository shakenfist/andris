#!/bin/bash
# Propose a release of andris: the first of a release's two stages.
#
# Creates a release-X.Y.Z branch from develop, bumps the version, runs
# pre-commit and the tests, and, after confirmation, commits and pushes
# the branch. It does not open a pull request and does not tag; both
# happen afterwards:
#
#   1. Run this script.
#   2. Open a pull request from release-X.Y.Z into develop, and land it
#      like any other change.
#   3. Run scripts/tag-release.sh X.Y.Z, which tags the merged commit
#      and pushes the tag; that is what starts
#      .github/workflows/release.yml.
#
# Usage:
#   scripts/propose-release.sh X.Y.Z
#   make propose-release X.Y.Z
#
# Andris has one version, [workspace.package]'s in the root Cargo.toml,
# which the crate inherits, plus the andris entry Cargo.lock records.
# The script edits the first and has `make lock` rewrite the second, so
# the lockfile is never edited by hand. Cargo runs only inside the
# devcontainer, through make, so the host needs no Rust toolchain.
#
# Requirements on the host: docker (for make lock and make test), git,
# curl, pre-commit and make. Adapted from ryll's tools/propose-release.sh.

set -euo pipefail

err() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

# --- arguments ---

[[ $# -eq 1 ]] || err "usage: $0 X.Y.Z (e.g. 0.1.0)"
VERSION="$1"
[[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || err "version must be X.Y.Z, got: ${VERSION}"

TAG="v${VERSION}"
RELEASE_BRANCH="release-${VERSION}"

for tool in git curl make pre-commit docker; do
    command -v "${tool}" >/dev/null || err "${tool} is not installed"
done

cd "$(dirname "$0")/.."
[[ -f Cargo.toml && -f andris/pyproject.toml ]] || err "could not find the repository root"

# --- git state ---

info "Checking git state"

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[[ "${BRANCH}" == "develop" ]] || err "must be on develop, currently on: ${BRANCH}"
[[ -z "$(git status --porcelain)" ]] || err "working tree is dirty; commit or stash first"

git fetch origin develop --quiet
[[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/develop)" ]] \
    || err "local develop is not in sync with origin/develop"

if git rev-parse --verify --quiet "refs/tags/${TAG}" >/dev/null; then
    err "tag ${TAG} already exists locally"
fi
if git ls-remote --tags --exit-code origin "refs/tags/${TAG}" >/dev/null 2>&1; then
    err "tag ${TAG} already exists on origin"
fi
if git rev-parse --verify --quiet "refs/heads/${RELEASE_BRANCH}" >/dev/null; then
    err "branch ${RELEASE_BRANCH} already exists locally"
fi
if git ls-remote --heads --exit-code origin "refs/heads/${RELEASE_BRANCH}" >/dev/null 2>&1; then
    err "branch ${RELEASE_BRANCH} already exists on origin"
fi

# --- the version must move forward ---

CURRENT="$(scripts/release-version.sh)"
[[ "${CURRENT}" != "${VERSION}" ]] || err "the workspace is already at ${VERSION}"
HIGHEST="$(printf '%s\n%s\n' "${CURRENT}" "${VERSION}" | sort -V | tail -n 1)"
[[ "${HIGHEST}" == "${VERSION}" ]] || err "${VERSION} is older than the current ${CURRENT}"

# --- PyPI ---
#
# PyPI never lets a version be uploaded twice, even after it is deleted,
# so a version it already has cannot be released. Its JSON API answers
# 404 for a version, or a project, it does not have.

info "Checking PyPI for andris ${VERSION}"
CODE="$(curl -s -o /dev/null -w '%{http_code}' \
    -A 'andris-release-script (https://github.com/shakenfist/andris)' \
    "https://pypi.org/pypi/andris/${VERSION}/json")"
case "${CODE}" in
    404) ;;
    200) err "andris ${VERSION} is already on PyPI" ;;
    *) err "unexpected HTTP ${CODE} asking PyPI about andris ${VERSION}" ;;
esac

# --- the release branch ---

info "Creating ${RELEASE_BRANCH} from develop"
git switch -c "${RELEASE_BRANCH}"

# Until the push succeeds, any exit puts the checkout back as it was.
CLEANUP_BRANCH=1
cleanup() {
    if [[ "${CLEANUP_BRANCH}" == "1" ]]; then
        info "Cleaning up: switching back to develop and deleting ${RELEASE_BRANCH}"
        git checkout -- . 2>/dev/null || true
        git switch develop 2>/dev/null || true
        git branch -D "${RELEASE_BRANCH}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

info "Running pre-commit"
pre-commit run --all-files

# --- bump ---

info "Bumping [workspace.package] version from ${CURRENT} to ${VERSION}"
BUMPED="$(mktemp)"
awk -v version="${VERSION}" '
    /^\[workspace\.package\]/ { in_wp = 1; print; next }
    /^\[/ { in_wp = 0 }
    in_wp && !done && /^version *= */ {
        print "version = \"" version "\""
        done = 1
        next
    }
    { print }
' Cargo.toml > "${BUMPED}"
cat "${BUMPED}" > Cargo.toml
rm -f "${BUMPED}"
[[ "$(scripts/release-version.sh)" == "${VERSION}" ]] || err "the version bump did not take"

# `make test`'s fetch passes --locked, so a lockfile still recording the
# old version would stop it. `make lock` is the one target that writes
# Cargo.lock; with no dependency changed, it moves only andris's entry.
info "Refreshing Cargo.lock"
make lock

# The release workflow's own check, run against the tag this release
# will have: Cargo.toml and Cargo.lock both at VERSION.
scripts/check-release-version.sh "refs/tags/${TAG}"

CHANGED="$(git diff --name-only | sort | tr '\n' ' ')"
[[ "${CHANGED}" == "Cargo.lock Cargo.toml " ]] \
    || err "expected the bump to change Cargo.toml and Cargo.lock only, but it changed: ${CHANGED}"

info "Running the tests (make test, in the devcontainer)"
make test

# --- confirm, commit and push ---

echo
echo "About to propose release ${VERSION} on branch ${RELEASE_BRANCH}."
git diff --stat
echo
read -rp "Commit and push ${RELEASE_BRANCH}? [y/N] " REPLY
[[ "${REPLY}" =~ ^[Yy]$ ]] || {
    info "Aborted at confirmation."
    exit 1
}

info "Committing"
git add Cargo.toml Cargo.lock
git commit -m "Release ${VERSION}."

info "Pushing ${RELEASE_BRANCH}"
git push --set-upstream origin "${RELEASE_BRANCH}"

# Pushed: leave the checkout on the release branch.
CLEANUP_BRANCH=0

echo
info "Release ${VERSION} proposed on ${RELEASE_BRANCH}."
echo
echo "Next steps:"
echo "  1. Open a pull request from ${RELEASE_BRANCH} into develop:"
echo "     https://github.com/shakenfist/andris/pull/new/${RELEASE_BRANCH}"
echo "  2. Land it as any other change."
echo "  3. Run 'make tag-release ${VERSION}' to tag the merged commit,"
echo "     which starts the release workflow."
