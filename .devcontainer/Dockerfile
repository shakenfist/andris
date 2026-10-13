# syntax=docker/dockerfile:1
FROM mcr.microsoft.com/devcontainers/base:debian

# The C toolchain and pkg-config. System libraries (libxcb and the like)
# arrive with the code that needs them, not before.
# Build tools:
#   - cmake: Build dependency for aws-lc-sys (via tokio-rustls, through
#     shakenfist-spice-protocol)
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    cmake \
    && rm -rf /var/lib/apt/lists/*

# A headless X server, to test the X11 backend against, and the X
# clients the end-to-end test (`make e2e`) runs on it:
#   - xvfb: the virtual framebuffer X server
#   - xauth: X authorisation cookies, for Xvfb -auth and xvfb-run
#   - x11-utils: xdpyinfo, which scripts/e2e.sh polls until Xvfb answers
#   - python3-xlib: scripts/e2e-keylogger.py's window (pulls in python3,
#     which scripts/e2e-drive.py also runs on)
#   - xclip: copies to and pastes from the X clipboard, for the clipboard
#     tests in andris/tests/x11.rs
#   - x11-xserver-utils: xrandr, to look at and change the screen size
# Ryll's slim headless build (`make ryll-headless`) needs nothing beyond
# the build tools above: cmake already serves aws-lc-sys, and its opus
# and openh264 crates build from bundled source.
RUN apt-get update && apt-get install -y \
    xvfb \
    xauth \
    x11-utils \
    python3-xlib \
    xclip \
    x11-xserver-utils \
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
