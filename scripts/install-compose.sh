#!/bin/bash
# Make `docker compose` available on a CI runner, for `make
# e2e-kerbside`. The vm pool's debian-13-docker image has the Docker
# engine and CLI but no Compose plugin. Debian 13's docker-compose
# package (2.26) installs it as a CLI plugin, which clears the 2.24
# that e2e-kerbside.sh checks for. Does nothing where `docker compose`
# already runs, so a runner image that gains the plugin costs nothing.
# Needs passwordless sudo, which vm runners have.

set -euo pipefail

if docker compose version >/dev/null 2>&1; then
    echo "[install-compose] $(docker compose version)"
    exit 0
fi

sudo apt-get update -q
sudo apt-get install -y -q --no-install-recommends docker-compose
echo "[install-compose] $(docker compose version)"
