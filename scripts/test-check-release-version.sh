#!/bin/bash
# Check scripts/check-release-version.sh against fixture Cargo.toml and
# Cargo.lock files, one pair for each case, in a temporary directory. Run
# by pre-commit whenever the script, or scripts/release-version.sh which
# it reads the workspace version with, changes.
#
# Usage: scripts/test-check-release-version.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK="${REPO_ROOT}/scripts/check-release-version.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
FAILURES=0

# fixture NAME TOML_VERSION LOCK_VERSION: write a Cargo.toml whose
# [workspace.package] has TOML_VERSION, and a Cargo.lock whose andris
# entry has LOCK_VERSION, into $WORK/NAME. A dependency whose name starts
# with andris comes first, with another version, so that only the andris
# entry itself can satisfy the check.
fixture() {
    local dir="${WORK}/$1"
    mkdir -p "${dir}"
    cat > "${dir}/Cargo.toml" <<EOF
[workspace]
members = ["andris"]

[workspace.package]
version = "$2"
edition = "2024"

[workspace.dependencies]
version = "9.9.9"
EOF
    cat > "${dir}/Cargo.lock" <<EOF
version = 4

[[package]]
name = "andris-helper"
version = "8.8.8"

[[package]]
name = "andris"
version = "$3"
dependencies = [
 "andris-helper",
]
EOF
}

indent() {
    local line
    while IFS= read -r line; do
        echo "    ${line}"
    done
}

# expect RESULT DESCRIPTION REF FIXTURE [TEXT]: run the check on REF
# against the fixture, and record a failure unless it passes (RESULT
# pass) or fails (RESULT fail), and, when TEXT is given, prints it.
expect() {
    local result="$1" description="$2" ref="$3" dir="${WORK}/$4" text="${5:-}"
    local output status=0 got
    output="$("${CHECK}" "${ref}" "${dir}" 2>&1)" || status=$?
    if [ "${status}" -eq 0 ]; then got=pass; else got=fail; fi
    if [ "${got}" != "${result}" ]; then
        echo "FAIL: ${description}: expected ${result}, got ${got} (status ${status}):"
        indent <<< "${output}"
        FAILURES=$((FAILURES + 1))
    elif [ -n "${text}" ] && ! grep -qF -- "${text}" <<< "${output}"; then
        echo "FAIL: ${description}: the output does not say '${text}':"
        indent <<< "${output}"
        FAILURES=$((FAILURES + 1))
    else
        echo "ok: ${description}"
    fi
}

fixture agree 1.2.3 1.2.3
fixture lock-behind 1.2.3 1.2.2
fixture empty '' ''
fixture no-lock-entry 1.2.3 1.2.3
sed -i 's/^name = "andris"$/name = "not-andris"/' "${WORK}/no-lock-entry/Cargo.lock"
fixture no-table 1.2.3 1.2.3
sed -i 's/^\[workspace\.package\]$/[package]/' "${WORK}/no-table/Cargo.toml"

expect pass 'a tag naming the version' refs/tags/v1.2.3 agree 'matches the workspace version'
expect fail 'a tag naming another version' refs/tags/v1.2.4 agree 'Version mismatch'
# The workflow publishes only from refs/tags/v*, so any other tag is a
# ref like a branch's: nothing to compare, and nothing published.
expect pass 'a tag without its v' refs/tags/1.2.3 agree 'Not a release tag'
expect fail 'Cargo.lock behind Cargo.toml, on a tag' refs/tags/v1.2.3 lock-behind 'run make lock'
expect fail 'Cargo.lock behind Cargo.toml, on a branch' refs/heads/develop lock-behind 'run make lock'
expect fail 'a tag naming no version' refs/tags/v agree 'Version mismatch'
expect fail 'an empty version, in files that agree' refs/heads/develop empty 'no [workspace.package] version'
expect fail 'no andris entry in Cargo.lock' refs/heads/develop no-lock-entry 'run make lock'
expect fail 'no [workspace.package] table' refs/tags/v1.2.3 no-table 'no [workspace.package] version'
expect pass 'a branch, with files that agree' refs/heads/develop agree 'Not a release tag'
expect pass 'a pull request ref, with files that agree' refs/pull/7/merge agree 'Not a release tag'

# release-version.sh reads standard input when tag-release.sh pipes
# `git show` into it. Fed more than a pipe buffer holds, an awk that
# exited at the version would SIGPIPE the writer, and pipefail would fail
# the pipeline with status 141.
if VERSION_READ="$( { cat "${WORK}/agree/Cargo.toml"; awk 'BEGIN { for (i = 0; i < 200000; i++) print "# padding" }'; } \
    | "${REPO_ROOT}/scripts/release-version.sh" -)" && [ "${VERSION_READ}" = 1.2.3 ]; then
    echo 'ok: release-version.sh reads a long piped Cargo.toml without SIGPIPE'
else
    echo 'FAIL: release-version.sh on a long piped Cargo.toml' >&2
    FAILURES=$((FAILURES + 1))
fi

if [ "${FAILURES}" -ne 0 ]; then
    echo "${FAILURES} check-release-version.sh case(s) failed" >&2
    exit 1
fi
echo 'check-release-version.sh passed every case'
