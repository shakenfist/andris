#!/bin/bash
# Runs inside the andris-wheel-<arch> image (.devcontainer/wheel/
# Dockerfile, on quay.io/pypa/manylinux_2_28_<arch>: AlmaLinux 8, glibc
# 2.28), with no network, the cargo cache read-only and the checkout
# read-only apart from target/. scripts/build-andris-wheel.sh runs it; it
# is not meant to be run on a normal host.
#
# It builds andris into a manylinux_2_28 wheel with maturin (bindings =
# "bin"), and checks two things before the wheel reaches target/wheels:
#
# - **The tag is earned.** maturin runs its policy check
#   (--auditwheel check) and refuses a binary that links a library
#   outside the manylinux_2_28 policy, or a glibc symbol newer than 2.28,
#   rather than repairing the wheel by bundling the library. Andris
#   loads libpulse at run time, so nothing needs an exception. The
#   binary's DT_NEEDED list is printed for the record.
# - **openh264's SIMD code went in.** Without nasm, the openh264 crate
#   silently builds without its x86 assembly, and encodes much slower.

set -euo pipefail

ARCH="${WHEEL_ARCH:?WHEEL_ARCH must name the architecture}"

export CARGO_TARGET_DIR="/work/target/wheel/${ARCH}"
rustc --version
maturin --version
nasm -v

# --frozen: build from Cargo.lock as fetched, and fail rather than reach
# for a network that is not there. The wheel is written to the
# container's own filesystem and copied into the checkout only once the
# checks below pass, so a failed check leaves no wheel behind.
WHEEL_DIR=/build/wheels
#
# When the policy check fails, maturin lists the libraries ("External
# shared libraries to be copied into the wheel") and suggests
# --auditwheel=repair. Do not take that advice: repairing bundles
# AlmaLinux 8's copy of the library into the wheel, where the host's own
# copy, and its configuration, belong. The message after it says so.
cd /work/andris
if ! maturin build --release --frozen \
    --compatibility manylinux_2_28 \
    --auditwheel check \
    --out "${WHEEL_DIR}"; then
    echo 'maturin failed. If it listed external shared libraries above, andris now links a library' >&2
    echo 'outside the manylinux_2_28 policy: load it at run time, as platform/pulse/ffi.rs loads' >&2
    echo 'libpulse, rather than bundling it with --auditwheel=repair.' >&2
    exit 1
fi
WHEELS=("${WHEEL_DIR}"/*.whl)
if [ "${#WHEELS[@]}" -ne 1 ] || [ ! -f "${WHEELS[0]}" ]; then
    echo "expected one wheel in ${WHEEL_DIR}, found: ${WHEELS[*]}" >&2
    exit 1
fi
WHEEL="${WHEELS[0]}"

# The binary the wheel ships, for the DT_NEEDED record: read from the
# wheel rather than from cargo's target directory, so the record is of
# the artifact. An empty list means nothing was read: every dynamically
# linked glibc binary needs libc.so.6.
BINARY="$(python3 -I - "${WHEEL}" /build/wheel-contents <<'EOF'
import sys
import zipfile

wheel, dest = sys.argv[1:]
with zipfile.ZipFile(wheel) as zf:
    names = [n for n in zf.namelist() if n.endswith('.data/scripts/andris')]
    if len(names) != 1:
        sys.exit(f'expected one andris binary in {wheel}, found {names}')
    print(zf.extract(names[0], dest))
EOF
)"
NEEDED="$(readelf -d "${BINARY}" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')"
if [ -z "${NEEDED}" ]; then
    echo "no DT_NEEDED entries read from ${BINARY}" >&2
    exit 1
fi
echo "DT_NEEDED of the wheel's andris: ${NEEDED//$'\n'/ }"

# The SIMD check. nm reads cargo's own binary, which keeps its symbol
# table whatever packaging later does to the wheel's copy. The symbol is
# WelsSampleSad16x16_<isa>, openh264's 16x16 sum of absolute
# differences, which its encoder installs in its SAD table on either
# architecture (codec/encoder/core/src/sample.cpp), so the linker keeps
# it: on x86_64 from nasm's codec/common/x86/satd_sad.asm, on aarch64
# from codec/encoder/core/arm64/pixel_aarch64_neon.S, which the C
# compiler assembles. It is defined (T) only when that assembly was
# built in. Both names were read from built binaries with nm (openh264-sys2
# 0.9.8), not guessed; on x86_64, building without nasm drops it.
case "${ARCH}" in
    x86_64) SIMD_SYMBOL=WelsSampleSad16x16_sse2 ;;
    aarch64) SIMD_SYMBOL=WelsSampleSad16x16_AArch64_neon ;;
    *)
        echo "no SIMD symbol known for ${ARCH}" >&2
        exit 1
        ;;
esac
TARGET_BINARY="${CARGO_TARGET_DIR}/release/andris"
SYMBOLS="$(nm --defined-only "${TARGET_BINARY}")"
if ! grep -Eq "[[:space:]][Tt][[:space:]]${SIMD_SYMBOL}\$" <<< "${SYMBOLS}"; then
    echo "openh264's SIMD code is missing: ${TARGET_BINARY} defines no ${SIMD_SYMBOL}." >&2
    echo 'Is nasm installed in the wheel image? Without it openh264 builds without its assembly.' >&2
    exit 1
fi
echo "openh264's SIMD code is in: ${TARGET_BINARY} defines ${SIMD_SYMBOL}"

cp "${WHEEL}" /work/target/wheels/
