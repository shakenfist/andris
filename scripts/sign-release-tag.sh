#!/bin/bash
# Install Sigstore's gitsign and replace the release tag with a keyless
# signed one, then force-push it. Run by release.yml's sign-tag job, on
# an ephemeral GitHub-hosted runner: it installs gitsign with sudo and
# writes global git configuration, so the machine must be thrown away
# afterwards. Adapted from ryll's tools/sign-release-tag.sh, which
# follows kerbside's sign-tag job.
#
# Keyless signing needs the job's OIDC token (id-token: write), and the
# push needs contents: write; the workflow grants both, to this job
# alone.
#
# Usage: scripts/sign-release-tag.sh TAG_NAME COMMIT_SHA
#
# The signature can be checked afterwards with
#   gitsign verify-tag TAG_NAME \
#     --certificate-identity-regexp '^https://github.com/shakenfist/andris/' \
#     --certificate-oidc-issuer https://token.actions.githubusercontent.com

set -euo pipefail

TAG_NAME="${1:?usage: sign-release-tag.sh TAG_NAME COMMIT_SHA}"
COMMIT_SHA="${2:?usage: sign-release-tag.sh TAG_NAME COMMIT_SHA}"

case "${TAG_NAME}" in
    v[0-9]*) ;;
    *)
        echo "refusing to sign ${TAG_NAME}: release tags are vX.Y.Z" >&2
        exit 1
        ;;
esac

# Kept in step with ryll's copy. The download is checked against the
# release's checksums.txt before it is installed.
GITSIGN_VERSION='0.14.0'
BINARY="gitsign_${GITSIGN_VERSION}_linux_amd64"
BASE="https://github.com/sigstore/gitsign/releases/download/v${GITSIGN_VERSION}"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
(
    cd "${WORK}"
    curl -fsSLO "${BASE}/${BINARY}"
    curl -fsSLO "${BASE}/checksums.txt"
    sha256sum --ignore-missing -c checksums.txt
    # --ignore-missing passes when no line names the file at all, so
    # check that one did.
    grep -q " ${BINARY}\$" checksums.txt
)
chmod +x "${WORK}/${BINARY}"
sudo mv "${WORK}/${BINARY}" /usr/local/bin/gitsign

git config --global user.name 'github-actions[bot]'
git config --global user.email 'github-actions[bot]@users.noreply.github.com'
git config --global tag.gpgsign true
git config --global gpg.format x509
git config --global gpg.x509.program gitsign

echo "Signing tag ${TAG_NAME} at ${COMMIT_SHA}"
git tag -d "${TAG_NAME}" || true
git tag -s "${TAG_NAME}" -m "Release ${TAG_NAME}" "${COMMIT_SHA}"
git push origin "${TAG_NAME}" --force
