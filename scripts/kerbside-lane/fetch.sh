#!/bin/bash
# Fetch kerbside's repository at a pinned commit, for `make e2e-kerbside`,
# which uses its demo stack (demo/) and the demo's own scripts.
#
# Usage: fetch.sh REV VERSION DIR
#
# REV is the full commit, VERSION the kerbside release whose tag, vVERSION,
# must name that commit: the fetch is of the tag, and fails unless the tag
# resolves to REV, so the two pins in the Makefile cannot drift apart. The
# tree goes in DIR/REV, and the fetch is skipped when that is already
# there. It is fetched into DIR/REV.tmp and renamed into place only once
# complete, as `make ryll-headless` stages ryll, so an interrupted fetch is
# never mistaken for a finished one.
#
# This needs the network, and runs git on the host. Fetching runs nothing
# from the repository: git runs no hooks a repository brings with it.
# The lane does run one of its scripts, the demo's get-console.sh, on the
# host, which is why a cached tree is checked before it is reused.

set -euo pipefail

REPOSITORY=https://github.com/shakenfist/kerbside

if [ "$#" -ne 3 ]; then
    echo "usage: $0 REV VERSION DIR" >&2
    exit 1
fi
rev=$1
version=$2
dir=$3

if ! echo "$rev" | grep -Eqx '[0-9a-f]{40}'; then
    echo "[kerbside-fetch] REV must be a full 40-character commit, not '$rev'" >&2
    exit 1
fi

tree=$dir/$rev
# A cached tree comes back from the CI cache, and the lane runs the
# demo's get-console.sh from it on the host, so it is used only while it
# is still exactly REV; otherwise it is fetched again.
if [ -f "$tree/demo/docker-compose.yml" ] \
    && [ "$(git -C "$tree" rev-parse HEAD 2>/dev/null)" = "$rev" ] \
    && [ -z "$(git -C "$tree" status --porcelain 2>/dev/null)" ]; then
    echo "[kerbside-fetch] kerbside $version already fetched: $tree"
    exit 0
fi

staging=$tree.tmp
rm -rf "$staging"
mkdir -p "$staging"
git -C "$staging" init --quiet
git -C "$staging" fetch --quiet --depth 1 "$REPOSITORY" "refs/tags/v$version"
tagged=$(git -C "$staging" rev-parse 'FETCH_HEAD^{commit}')
if [ "$tagged" != "$rev" ]; then
    echo "[kerbside-fetch] kerbside's tag v$version names $tagged, not the pinned $rev;" >&2
    echo "  bump KERBSIDE_REV and KERBSIDE_VERSION in the Makefile together" >&2
    rm -rf "$staging"
    exit 1
fi
git -C "$staging" -c advice.detachedHead=false checkout --quiet "$rev"
rm -rf "$tree"
mv "$staging" "$tree"
echo "[kerbside-fetch] kerbside $version fetched: $tree"
