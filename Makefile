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
	target-dir lint lint-fix test help

all: build

help:
	@echo "Andris build targets:"
	@echo "  make devcontainer           - Build the development container"
	@echo "  make fetch                  - Pre-download crates into the cargo cache"
	@echo "  make lock                   - Refresh Cargo.lock after a dependency change"
	@echo "  make build                  - Build debug version"
	@echo "  make release                - Build release version"
	@echo "  make test                   - Run tests"
	@echo "  make lint                   - Run rustfmt and clippy checks"
	@echo "  make lint-fix               - Run rustfmt and clippy with auto-fix"
	@echo "  make clean                  - Remove build artifacts"

# Build the devcontainer image
devcontainer:
	docker build -t $(ANDRIS_IMAGE) $(DEVCONTAINER_DIR)

# Create cargo cache directories
$(CARGO_CACHE)/registry $(CARGO_CACHE)/git:
	mkdir -p $@

# Ensure cargo cache directories are writable by the build user.
# A previous root-owned docker run can leave these owned by root.
ensure-cache: devcontainer $(CARGO_CACHE)/registry $(CARGO_CACHE)/git
	@if [ ! -w "$(CARGO_CACHE)/registry" ] || [ ! -w "$(CARGO_CACHE)/git" ]; then \
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

# Clean build artifacts.
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
