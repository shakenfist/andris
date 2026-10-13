# syntax=docker/dockerfile:1
# The xrdp test bed for `make e2e-xrdp`: a real xrdp session, which andris
# shares, inside a throwaway container rather than on anyone's desktop. See
# scripts/e2e-xrdp.sh, which builds this image, and run.sh, which runs in it.
#
# Built from the devcontainer image (`make devcontainer`), so its glibc is
# the one the andris and ryll binaries were built against, and its Xvfb,
# xauth, x11-utils, python3-xlib, xclip and x11-xserver-utils are already
# there.
FROM andris-dev

# The RDP server and client, from Debian 13, at the versions the operator's
# host runs:
#   - xrdp: the RDP server, xrdp and xrdp-sesman
#   - xorgxrdp: the Xorg modules xrdp's sessions run on (pulls in Xorg)
#   - freerdp3-x11: xfreerdp3, the RDP client, run on an Xvfb of its own
#   - xdotool: types into, and resizes, the FreeRDP window
#   - dbus-x11: dbus-launch, which xrdp's session scripts expect
#   - python3-xlib: the keylogger
#   - xclip: copies to and pastes from CLIPBOARD on either display
#   - x11-xserver-utils: xrandr, to look at and change the screen size
# The last three are in the base image already; they are listed so that
# this image does not depend on that.
RUN apt-get update && apt-get install -y --no-install-recommends \
    xrdp \
    xorgxrdp \
    freerdp3-x11 \
    xdotool \
    dbus-x11 \
    python3-xlib \
    xclip \
    x11-xserver-utils \
    && rm -rf /var/lib/apt/lists/*

# The marker every test-bed script checks before it does anything, so that
# none of them can run against a real desktop by mistake.
RUN echo 'andris xrdp test bed; see scripts/xrdp-testbed/Dockerfile' >/etc/andris-xrdp-testbed

# The versions, for the build log; run.sh prints them again on every run.
RUN dpkg-query -W xrdp xorgxrdp freerdp3-x11
