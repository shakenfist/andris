# Andris - a SPICE server that exports an existing X11 desktop
# Build and development targets. Everything runs in the devcontainer;
# Docker is the only host dependency.

ANDRIS_IMAGE := andris-dev
DEVCONTAINER_DIR := .devcontainer
# Cargo download cache, bind-mounted into the devcontainer so crates are
# not re-downloaded on every build. Override CARGO_CACHE to point at a
# location outside the checkout that survives across CI runs (the
# in-checkout default is deleted by `actions/checkout`, whose default
# `clean: true` runs `git clean -ffdx` and so removes this gitignored
# directory every run). Resolved to an absolute path so an override may
# itself be absolute. `make clean` only removes the cache when it lies
# inside the checkout, so pointing this at a shared directory is safe.
CARGO_CACHE ?= .cargo-cache
# `?=` treats CARGO_CACHE= in the environment as set, so an override fed
# by an unset CI variable leaves this empty. Every target degrades badly
# on that -- the mounts become -v "/registry":..., the mkdir rule becomes
# `mkdir -p /registry`, and `clean` would expand to `rm -rf /` -- and
# the resulting permission error names none of it. One check at parse
# time covers the lot.
ifeq ($(strip $(CARGO_CACHE)),)
$(error CARGO_CACHE is empty; unset it or give it a path)
endif
CARGO_CACHE_DIR := $(abspath $(CARGO_CACHE))

# The ryll revision Cargo.lock pins, for `make ryll-headless`: the full
# sha after the `#` in the shakenfist-spice-protocol package's source
# line, so the lockfile stays the one source of truth. The regex matches
# the `#` with `.` because a `#` in a makefile line starts a comment in
# some versions of make and not in others.
RYLL_REV := $(shell sed -nE '/^name = "shakenfist-spice-protocol"$$/,/^$$/ s/^source = "git\+https:\/\/github\.com\/shakenfist\/ryll\?rev=[0-9a-f]+.([0-9a-f]{40})"$$/\1/p' Cargo.lock)
# Ryll is installed per revision under the cargo cache, so CI's cache
# action keeps the binary between runs and a new pin builds afresh.
RYLL_DIR := $(CARGO_CACHE_DIR)/ryll
RYLL_BIN := $(RYLL_DIR)/$(RYLL_REV)/bin/ryll

# Kerbside, for `make e2e-kerbside`: the commit whose demo stack (demo/)
# the lane runs, and the release on PyPI that commit is tagged as, which
# the demo's image installs. Renovate does not track either. To bump
# them, take a newer kerbside release, set KERBSIDE_VERSION to it and
# KERBSIDE_REV to the full commit its tag v<version> names, together:
# the fetch fails if the tag names another commit. Kerbside is fetched
# per commit under the cargo cache, as ryll is, so CI's cache keeps it.
KERBSIDE_REV := 2d9ee91e29095e4566b8452d50fbc1ca521ba4f8
KERBSIDE_VERSION := 0.7.0
KERBSIDE_ROOT := $(CARGO_CACHE_DIR)/kerbside

# The architecture `make wheel` builds for: the host's by default, since
# the wheel is built natively, not under emulation. Set on the command
# line (`make wheel ARCH=x86_64`), which overrides this; an ARCH in the
# environment does not, since some build environments export one of
# their own (amd64, say).
ARCH := $(shell uname -m)

# Detect user/group for permission-safe container builds
UID := $(shell id -u)
GID := $(shell id -g)

# Shared pieces of the devcontainer docker-run invocation.
# CARGO_BUILD_JOBS is forwarded only when set in the caller's
# environment, so parallelism can be bounded on small machines
# (docker omits the variable entirely when it is unset on the
# host). Targets append the image name and command. The workspace and
# cache mounts are split out because the offline compile takes them
# read-only; see the security model below.
DOCKER_BASE_ARGS := \
	-w /workspace \
	-u $(UID):$(GID) \
	-e HOME=/build \
	-e CARGO_BUILD_JOBS

# The checkout, writable in full: for the targets that compile nothing
# (`fetch`, `lock`) and for `lint-fix`, which has to write sources.
WORKSPACE_MOUNT_RW := -v "$(CURDIR)":/workspace
# The checkout read-only, with only target/ writable on top of it. The
# target/ bind mount needs its host directory to exist first -- docker
# cannot create the mountpoint inside a read-only mount -- which is what
# the target-dir prerequisite is for.
WORKSPACE_MOUNT_RO := \
	-v "$(CURDIR)":/workspace:ro \
	-v "$(CURDIR)/target":/workspace/target

CACHE_MOUNTS := \
	-v "$(CARGO_CACHE_DIR)/registry":/build/.cargo/registry \
	-v "$(CARGO_CACHE_DIR)/git":/build/.cargo/git
CACHE_MOUNTS_RO := \
	-v "$(CARGO_CACHE_DIR)/registry":/build/.cargo/registry:ro \
	-v "$(CARGO_CACHE_DIR)/git":/build/.cargo/git:ro

# Networked invocation with a writable checkout and cache, for the
# targets that compile nothing: `fetch` and `lock`, which resolve and
# download only. `ensure-cache`'s permission fix is not one of these: it
# needs a root container, so it writes its own docker run.
DOCKER_RUN := docker run --rm $(DOCKER_BASE_ARGS) $(WORKSPACE_MOUNT_RW) $(CACHE_MOUNTS)

# Networked invocation for `ryll-headless`, which compiles ryll and its
# dependency tree with the network up. It is not DOCKER_RUN: that mounts
# the checkout and the shared crate cache writable, which is safe only
# because nothing compiles under it. This mounts neither, only the one
# per-revision staging directory, $(RYLL_DIR)/$(RYLL_REV).tmp, and the
# recipe points CARGO_HOME and the install root into it, so ryll's build
# scripts can reach nothing that andris's own compiles later trust: not
# .git, not the sources, not the crates the offline targets read, and not
# the ryll binary of any other revision, which a build script could
# otherwise plant for a later `make e2e` to run. The rename of the staging
# directory into place happens on the host, outside the container.
DOCKER_RUN_RYLL := docker run --rm \
	-w /build/ryll \
	-u $(UID):$(GID) \
	-e HOME=/build \
	-e CARGO_BUILD_JOBS \
	-e CARGO_HOME=/build/ryll/cargo-home \
	-v "$(RYLL_DIR)/$(RYLL_REV).tmp":/build/ryll

# Offline invocation for the targets that compile crates: build,
# release, test and lint. A build script (build.rs) runs arbitrary code
# at compile time for each dependency in the tree -- the supply-chain
# attack surface. Three defences:
#
# - --network none severs the network namespace, so a compromised
#   dependency cannot reach a C2 or exfiltrate secrets (its download
#   call fails and the build aborts loudly).
# - The cargo cache is mounted read-only, so a build script cannot
#   poison it for later runs.
# - The checkout is mounted read-only with only target/ writable. The
#   cache mount alone is not enough: the default cache, .cargo-cache,
#   lives inside the checkout, so a writable /workspace would let a
#   build script rewrite /workspace/.cargo-cache (which actions/cache
#   then persists for every later run), or plant core.fsmonitor or
#   core.hooksPath in /workspace/.git/config, which runs on the host the
#   next time anything there runs git -- pre-commit's own `git diff`,
#   say. Sources, Cargo.lock, .git and the cache are all out of reach.
#
# Crates must be pre-populated by `make fetch`, the one target allowed
# network, which every offline target below depends on. What a build
# script can still write is target/, so treat target/ as being exactly
# as trustworthy as the code that was compiled into it.
DOCKER_RUN_OFFLINE := docker run --rm --network none \
	$(DOCKER_BASE_ARGS) $(WORKSPACE_MOUNT_RO) $(CACHE_MOUNTS_RO)

# Offline, read-only cache, but a WRITABLE checkout: for `lint-fix`
# only, because rustfmt and `clippy --fix` rewrite sources in place, and
# clippy compiles the dependency tree to do it. Its build scripts
# therefore run with the whole checkout writable -- including .git and
# an in-checkout .cargo-cache -- so the protection above does not apply.
# Run `make lint-fix` only on code you trust; CI never runs it.
DOCKER_RUN_OFFLINE_RW := docker run --rm --network none \
	$(DOCKER_BASE_ARGS) $(WORKSPACE_MOUNT_RW) $(CACHE_MOUNTS_RO)

.PHONY: all build release clean devcontainer ensure-cache fetch lock \
	target-dir lint lint-fix test ryll-headless e2e e2e-xrdp \
	kerbside-demo e2e-kerbside wheel propose-release tag-release help

all: build

help:
	@echo "Andris build targets:"
	@echo "  make devcontainer           - Build the development container"
	@echo "  make fetch                  - Pre-download crates into the cargo cache"
	@echo "  make lock                   - Refresh Cargo.lock after a dependency change"
	@echo "  make build                  - Build debug version"
	@echo "  make release                - Build release version"
	@echo "  make test                   - Run tests"
	@echo "  make ryll-headless          - Build ryll's headless client at the pinned revision"
	@echo "  make e2e                    - Run the end-to-end test against Xvfb and ryll"
	@echo "  make e2e-xrdp               - Run andris against a real xrdp session, in a container"
	@echo "  make e2e-kerbside           - Run the end-to-end checks through kerbside's demo stack"
	@echo "  make wheel [ARCH=x86_64]    - Build the manylinux_2_28 wheel for PyPI into target/wheels"
	@echo "  make propose-release X.Y.Z  - Branch, bump the version, test and push for review"
	@echo "  make tag-release X.Y.Z      - After the release PR lands: tag develop, start the release"
	@echo "  make lint                   - Run rustfmt and clippy checks"
	@echo "  make lint-fix               - Run rustfmt and clippy with auto-fix"
	@echo "  make clean                  - Remove build artifacts"

# Build the devcontainer image
devcontainer:
	docker build -t $(ANDRIS_IMAGE) $(DEVCONTAINER_DIR)

# Create cargo cache directories
$(CARGO_CACHE)/registry $(CARGO_CACHE)/git $(CARGO_CACHE)/ryll:
	mkdir -p $@

# Ensure cargo cache directories are writable by the build user.
# A previous root-owned docker run can leave these owned by root.
ensure-cache: devcontainer $(CARGO_CACHE)/registry $(CARGO_CACHE)/git $(CARGO_CACHE)/ryll
	@if [ ! -w "$(CARGO_CACHE)/registry" ] || [ ! -w "$(CARGO_CACHE)/git" ] \
			|| [ ! -w "$(CARGO_CACHE)/ryll" ]; then \
		echo "Fixing cargo cache permissions..."; \
		docker run --rm \
			-v "$(CARGO_CACHE_DIR)":/cache \
			$(ANDRIS_IMAGE) \
			chown -R $(UID):$(GID) /cache; \
	fi

# Populate the cargo cache. This is the ONLY build target permitted
# network access. `cargo fetch` downloads every crate named in
# Cargo.lock but compiles nothing, so no build script runs here -- the
# untrusted code only executes later, offline, in the compile targets.
# --locked additionally refuses to proceed if Cargo.lock is stale.
fetch: ensure-cache
	$(DOCKER_RUN) $(ANDRIS_IMAGE) cargo fetch --locked

# Refresh Cargo.lock after editing a Cargo.toml. The only target
# allowed to write the lockfile: `fetch` passes --locked and every
# compile passes --frozen, so without this there is no in-devcontainer
# path to a lockfile update at all, and the first `make build` after
# adding or bumping a dependency fails at `fetch` with "the lock file
# needs to be updated but --locked was passed". Still runs no build
# script -- `cargo fetch` resolves and downloads, it does not compile.
lock: ensure-cache
	$(DOCKER_RUN) $(ANDRIS_IMAGE) cargo fetch

# Create target/ on the host so the offline targets can mount it
# writable over their read-only checkout. Refuse a symlink: target/ is
# gitignored only as a directory, so a branch can commit a `target`
# symlink, and mounting that writable would hand a build script
# whatever it points at -- .git, say -- and undo the read-only mount.
target-dir:
	@if [ -L target ]; then \
		echo "target is a symlink; refusing to mount it writable" >&2; \
		exit 1; \
	fi
	mkdir -p target

# Build debug version
build: fetch target-dir
	$(DOCKER_RUN_OFFLINE) \
		$(ANDRIS_IMAGE) \
		cargo build --frozen --workspace

# Build release version
release: fetch target-dir
	$(DOCKER_RUN_OFFLINE) \
		$(ANDRIS_IMAGE) \
		cargo build --release --frozen --workspace

# Run tests
test: fetch target-dir
	$(DOCKER_RUN_OFFLINE) \
		$(ANDRIS_IMAGE) \
		cargo test --frozen --workspace

# Run linting checks (rustfmt + clippy)
lint: fetch target-dir
	$(DOCKER_RUN_OFFLINE) \
		$(ANDRIS_IMAGE) \
		sh -c "cargo fmt --all -- --check && cargo clippy --frozen --workspace --all-targets -- -D warnings"

# Run linting with auto-fix. Unlike the targets above this mounts the
# checkout writable, since rustfmt and clippy --fix write sources: run
# it only on code you trust (see DOCKER_RUN_OFFLINE_RW). clippy --fix
# refuses to edit files outside a repository it can see, and in a git
# worktree .git is a file pointing outside the container's mount, so
# --allow-no-vcs is needed; review the diff with git on the host.
lint-fix: fetch
	$(DOCKER_RUN_OFFLINE_RW) \
		$(ANDRIS_IMAGE) \
		sh -c "cargo fmt --all && cargo clippy --fix --frozen --allow-dirty --allow-no-vcs --workspace --all-targets -- -D warnings"

# Build ryll's slim headless client (no GUI or capture, but with audio,
# so its playback channel opens) at the revision Cargo.lock pins, for
# `make e2e`. Skipped when that revision's
# binary already exists. Installed into <rev>.tmp, the only directory the
# build container sees, and renamed into place on the host only once
# complete, so an interrupted build is never mistaken for a finished one.
# The staging directory is created here first, so docker does not create
# it owned by root. CARGO_HOME sits inside it, apart from andris's crate
# cache (see DOCKER_RUN_RYLL), and is removed once the binary is
# installed, so the cache keeps only the binary.
ryll-headless: ensure-cache
	@echo "$(RYLL_REV)" | grep -Eqx '[0-9a-f]{40}' || { \
		echo "could not read ryll's revision from Cargo.lock (got '$(RYLL_REV)')" >&2; \
		exit 1; \
	}
	@echo "ryll revision $(RYLL_REV)"
	@if [ -x "$(RYLL_BIN)" ]; then \
		echo "ryll already built: $(RYLL_BIN)"; \
	else \
		rm -rf "$(RYLL_DIR)/$(RYLL_REV).tmp" && \
		mkdir -p "$(RYLL_DIR)/$(RYLL_REV).tmp" && \
		$(DOCKER_RUN_RYLL) \
			$(ANDRIS_IMAGE) \
			cargo install --locked --git https://github.com/shakenfist/ryll \
				--rev $(RYLL_REV) --no-default-features \
				--features audio ryll \
				--root /build/ryll && \
		rm -rf "$(RYLL_DIR)/$(RYLL_REV).tmp/cargo-home" "$(RYLL_DIR)/$(RYLL_REV)" && \
		mv "$(RYLL_DIR)/$(RYLL_REV).tmp" "$(RYLL_DIR)/$(RYLL_REV)" && \
		echo "ryll built: $(RYLL_BIN)"; \
	fi

# Run andris against Xvfb and drive it with ryll's headless client; see
# scripts/e2e.sh. Offline, with the checkout read-only, like the compile
# targets: the script writes only under the container's /tmp, and cargo
# only under target/. Ryll's binary is mounted read-only from the cargo
# cache. The X11 backend's test (andris/tests/x11.rs) and the PulseAudio
# capture's (andris/tests/pulse.rs), which the script runs against its
# Xvfb and its PulseAudio daemon, are compiled first, so that the
# script's own runs of them cost only the tests.
e2e: build ryll-headless
	$(DOCKER_RUN_OFFLINE) \
		-v "$(dir $(RYLL_BIN))":/build/ryll-bin:ro \
		-e RYLL=/build/ryll-bin/ryll \
		-e ANDRIS=/workspace/target/debug/andris \
		$(ANDRIS_IMAGE) \
		sh -c "cargo test --frozen --no-run --test x11 --test pulse && scripts/e2e.sh"

# Fetch kerbside's repository at KERBSIDE_REV, for its demo stack; see
# scripts/kerbside-lane/fetch.sh. Needs the network, and is skipped when
# that commit is already there.
kerbside-demo:
	scripts/kerbside-lane/fetch.sh $(KERBSIDE_REV) $(KERBSIDE_VERSION) "$(KERBSIDE_ROOT)"

# Run the end-to-end checks with ryll connected through kerbside's demo
# stack, so that every channel crosses kerbside's allowlist; see
# scripts/e2e-kerbside.sh. It builds the demo's images with the network
# up, then runs the stack on an internal compose network, and the test
# container, from the devcontainer image, in the kerbside container's
# network namespace, with the checkout read-only as `e2e` has it. It
# always removes the stack (`docker compose down -v`) when it ends.
e2e-kerbside: build ryll-headless kerbside-demo
	ANDRIS_IMAGE=$(ANDRIS_IMAGE) RYLL="$(RYLL_BIN)" ANDRIS=target/debug/andris \
		KERBSIDE_DIR="$(KERBSIDE_ROOT)/$(KERBSIDE_REV)" KERBSIDE_VERSION=$(KERBSIDE_VERSION) \
		scripts/e2e-kerbside.sh

# Share a real xrdp session with andris, in a throwaway container built
# from the devcontainer image, and drive it with ryll's headless client;
# see scripts/e2e-xrdp.sh. Local only, not run in CI. The release build is
# the one tested, as the one an operator runs alongside xrdp.
e2e-xrdp: release ryll-headless
	RYLL="$(RYLL_BIN)" ANDRIS=target/release/andris scripts/e2e-xrdp.sh

# Build andris into a manylinux_2_28 wheel for ARCH, in target/wheels;
# see scripts/build-andris-wheel.sh. It uses its own image, built on
# the manylinux base, rather than the devcontainer, and splits the
# build as the targets above do: `cargo fetch` with the network up,
# then the compile with --network none, the cargo cache read-only and
# the checkout read-only apart from target/.
wheel:
	CARGO_CACHE="$(CARGO_CACHE_DIR)" scripts/build-andris-wheel.sh $(ARCH)

# A release is two stages; see docs/releasing.md. First, `make
# propose-release X.Y.Z` runs scripts/propose-release.sh, which creates a
# release-X.Y.Z branch off develop, bumps the version (refreshing
# Cargo.lock through `make lock`), tests, and pushes the branch for a pull
# request. Then, once that pull request has landed, `make tag-release
# X.Y.Z` runs scripts/tag-release.sh, which tags the merged commit and
# pushes the tag, starting .github/workflows/release.yml.
#
# The version is the second word on the command line. The rule below
# absorbs it as a do-nothing target, so make does not complain that it
# has no rule to make "0.1.0"; it applies only when one of these two
# targets is asked for, so a mistyped target still fails.
RELEASE_VERSION := $(word 2,$(MAKECMDGOALS))

propose-release:
	@if [ -z "$(RELEASE_VERSION)" ]; then \
		echo "usage: make propose-release X.Y.Z" >&2; exit 1; \
	fi
	scripts/propose-release.sh $(RELEASE_VERSION)

tag-release:
	@if [ -z "$(RELEASE_VERSION)" ]; then \
		echo "usage: make tag-release X.Y.Z" >&2; exit 1; \
	fi
	scripts/tag-release.sh $(RELEASE_VERSION)

ifneq ($(filter propose-release tag-release,$(MAKECMDGOALS)),)
$(RELEASE_VERSION):
	@:
endif

# Clean build artifacts. target/ holds the wheels too.
#
# The cargo cache is only removed when it lives inside the checkout.
# CARGO_CACHE may point at a shared directory that outlives this
# checkout (see its comment at the top of this file), and `clean` has
# no business deleting that. An empty CARGO_CACHE is rejected at parse
# time, so this recipe only has to decide in-tree versus out-of-tree.
clean:
	rm -rf target
	@case "$(CARGO_CACHE_DIR)/" in \
		"$(CURDIR)"/?*) rm -rf "$(CARGO_CACHE_DIR)/" ;; \
		*) echo "Kept $(CARGO_CACHE_DIR) (not below $(CURDIR); delete by hand)" ;; \
	esac
