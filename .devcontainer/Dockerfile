# syntax=docker/dockerfile:1
FROM mcr.microsoft.com/devcontainers/base:debian

# Only the C toolchain and pkg-config. System libraries (libxcb and the
# like) arrive with the code that needs them, not before.
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Create a shared build directory that any user can use
RUN mkdir -p /build && chmod 777 /build

# Install Rust stable via rustup into /build
# Use umask 0000 so directories are world-writable (any user can write to cargo cache)
ENV CARGO_HOME=/build/.cargo
ENV RUSTUP_HOME=/build/.rustup
RUN umask 0000 && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable

# Add cargo to PATH
ENV PATH="/build/.cargo/bin:${PATH}"

# Install useful cargo tools
RUN umask 0000 && rustup component add rustfmt clippy

# Verify installation
RUN rustc --version && cargo --version
