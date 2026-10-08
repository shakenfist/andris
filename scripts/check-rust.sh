#!/bin/bash
# Run rustfmt and clippy over the workspace. Used by pre-commit.
#
# Usage: scripts/check-rust.sh [check|fix]
#
# This deliberately delegates to the Makefile rather than invoking docker
# itself. `make lint` and `make lint-fix` already build the andris-dev
# image, fix cache ownership and run offline, so the hook and the
# developer's `make lint` cannot drift apart.

set -e

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

case "${1:-check}" in
    check) exec make lint ;;
    fix) exec make lint-fix ;;
    *)
        echo "Usage: $0 [check|fix]" >&2
        exit 2
        ;;
esac
