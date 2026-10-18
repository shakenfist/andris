# syntax=docker/dockerfile:1
# The wheel build image: the manylinux_2_28 base (AlmaLinux 8, glibc
# 2.28), with a C toolchain, nasm, a Rust toolchain and maturin. Built by
# scripts/build-andris-wheel.sh, which then runs `cargo fetch` in it with
# the network up, and the maturin compile in it with the network severed.
# Everything that needs a download therefore happens here, when the image
# is built, because the compile cannot reach the network.
ARG ARCH
FROM quay.io/pypa/manylinux_2_28_${ARCH}

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
# toolchain is stable, as the devcontainer's is.
RUN mkdir -p /build && chmod 777 /build
ENV CARGO_HOME=/build/.cargo
ENV RUSTUP_HOME=/build/.rustup
RUN umask 0000 && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/build/.cargo/bin:/opt/python/cp311-cp311/bin:${PATH}"

# 1.7.1 is the first maturin with --auditwheel, which the build uses.
RUN pip install --quiet 'maturin>=1.7.1,<2'

RUN rustc --version && cargo --version && maturin --version && nasm -v
