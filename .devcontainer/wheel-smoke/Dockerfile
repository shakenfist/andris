# syntax=docker/dockerfile:1
# The image scripts/wheel-smoke.sh installs the wheel into: a stock
# python:3-slim, which has no libpulse, no X server and no systemd, like a
# slim container or a minimal server. It adds nothing, so that the
# smoke test sees the wheel on a bare host.
#
# Pinned by digest as well as by tag: the tag is for a reader, and the
# digest is what pins. Renovate's dockerfile manager reads this FROM line,
# and proposes a new digest when the tag moves.
FROM python:3-slim@sha256:2da676046630d7bf14c6764ae1143dc7fa0377a53f94bf5a642f0b214aca5bce
