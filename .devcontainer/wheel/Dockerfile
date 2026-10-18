# syntax=docker/dockerfile:1
# check=skip=InvalidDefaultArgInFrom
# The wheel build image: the manylinux_2_28 base (AlmaLinux 8, glibc
# 2.28), with a C toolchain, nasm, a Rust toolchain and maturin. Built by
# scripts/build-andris-wheel.sh, which then runs `cargo fetch` in it with
# the network up, and the maturin compile in it with the network severed.
# Everything that needs a download therefore happens here, when the image
# is built, because the compile cannot reach the network.
#
# The wheel is a published artifact, so the toolchain that builds it is
# pinned: the base image by digest, Rust and maturin by exact version.
# Renovate proposes each bump: the dockerfile manager reads the FROM
# lines, the dockerfileVersions preset the annotated _VERSION ARGs, and
# its pip_requirements manager requirements.txt and its hashes.
ARG ARCH

# One stage for each architecture's base, so that each FROM line names
# one image and digest that Renovate can read. The tag is for a reader;
# the digest is what pins. BuildKit builds only the stage that the last
# FROM selects, so only the host's architecture is pulled. ARCH has no
# default, so that a build that does not name one fails rather than
# picks one; the check skipped at the top warns of exactly that.
FROM quay.io/pypa/manylinux_2_28_x86_64:latest@sha256:1e7b8460ad8fb3fb0fbe1cd818845159ac896b4fd17452d56e892c291f3b97a8 AS base-x86_64
FROM quay.io/pypa/manylinux_2_28_aarch64:latest@sha256:b873ffc316ffe8a154e3dc4872871de2ebd170db4876c8bbd983d42ff320001e AS base-aarch64

FROM base-${ARCH}

# Andris links no system library beyond glibc and the C++ runtime
# (libpulse is loaded at run time), so there are no -devel packages:
#   - gcc, gcc-c++, make, cmake and pkgconf: aws-lc-sys (through
#     tokio-rustls), openh264 (C++) and opusic-sys's bundled libopus;
#   - nasm: openh264's x86 assembly. Without it the openh264 crate
#     silently skips its SIMD code, and a 1080p encode takes about 2.7x
#     as long; scripts/build-andris-wheel-in-container.sh checks that it
#     went in. EL8 ships nasm in PowerTools (CodeReady Builder on RHEL),
#     which is named here rather than trusted to be enabled already.
RUN dnf -y --enablerepo=powertools install \
        gcc gcc-c++ make cmake pkgconf-pkg-config nasm \
    && dnf clean all

# The build runs as the invoking host user, which has no passwd entry
# here, so the toolchain lives in a world-writable /build, as in the
# andris-dev devcontainer. umask 0000 keeps what rustup writes there
# writable too: cargo takes its package-cache lock under CARGO_HOME. The
# devcontainer follows stable; this pins one stable release, and
# Renovate proposes each new one.
RUN mkdir -p /build && chmod 777 /build
ENV CARGO_HOME=/build/.cargo
ENV RUSTUP_HOME=/build/.rustup
# renovate: datasource=github-releases depName=rust-lang/rust
ARG RUST_VERSION=1.99.1
RUN umask 0000 && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain "${RUST_VERSION}" --profile minimal
ENV PATH="/build/.cargo/bin:/opt/python/cp311-cp311/bin:${PATH}"

# maturin, at an exact version and checked against PyPI's hashes for it.
# The build needs 1.7.1 or newer, the first with --auditwheel.
COPY requirements.txt /build/maturin-requirements.txt
RUN pip install --quiet --require-hashes -r /build/maturin-requirements.txt

RUN rustc --version && cargo --version && maturin --version && nasm -v
