#!/bin/sh
# The demo desktop: Xvfb on :1, a session bus at a fixed address (so commands
# run through `lnx` reach the same AT-SPI registry as the apps), Openbox, and
# the cua-driver daemon. Runs as the desktop user under cua-desktop.service;
# if any piece exits, the script exits and systemd restarts the lot.
set -eu

export DISPLAY=:1
export XDG_RUNTIME_DIR=/run/cua-desktop
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
export NO_AT_BRIDGE=0
export GTK_A11Y=atspi
export HOME="${HOME:-/home/$(id -un)}"

rm -f "$XDG_RUNTIME_DIR/bus" /tmp/.X1-lock /tmp/.X11-unix/X1

Xvfb :1 -screen 0 1280x800x24 -nolisten tcp -dpi 96 &
xvfb=$!
for _ in $(seq 1 100); do
  xdpyinfo >/dev/null 2>&1 && break
  sleep 0.1
done
xdpyinfo >/dev/null

dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --nofork --nopidfile &
bus=$!
for _ in $(seq 1 100); do
  [ -S "$XDG_RUNTIME_DIR/bus" ] && break
  sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/bus" ]

openbox &
wm=$!
# Openbox clears the root window when it takes over the screen, so paint it
# once Openbox has announced itself.
for _ in $(seq 1 100); do
  xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id' && break
  sleep 0.1
done
xsetroot -solid '#1e1e2e'

CUA_DRIVER_RS_UPDATE_CHECK=0 cua-driver serve &
driver=$!

# The demo page; cua-driver navigates only to http(s) URLs.
python3 -m http.server --bind 127.0.0.1 --directory /opt/cua-demo/www 8000 >/dev/null 2>&1 &
web=$!

# Exit (and let systemd restart everything) as soon as one piece dies.
while kill -0 "$xvfb" "$bus" "$wm" "$driver" "$web" 2>/dev/null; do
  sleep 1
done
kill "$xvfb" "$bus" "$wm" "$driver" "$web" 2>/dev/null || true
exit 1
