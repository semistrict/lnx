#!/bin/sh
set -eu

export DEBIAN_FRONTEND=noninteractive

log() {
  printf 'browser-setup: %s\n' "$*"
}

apt_get() {
  sudo timeout "${LNX_BROWSER_SETUP_APT_TIMEOUT:-180s}" apt-get \
    -o Acquire::http::Timeout=15 \
    -o Acquire::https::Timeout=15 \
    -o Acquire::Retries=2 \
    -o Dpkg::Lock::Timeout=60 \
    "$@"
}

show_failure_logs() {
  sudo systemctl status --no-pager lnx-browser-test.service snapd.socket snapd.service >&2 || true
  sudo journalctl -u lnx-browser-test.service --no-pager -n 80 >&2 || true
  [ -f /tmp/lnx-cage.log ] && { printf '\n--- /tmp/lnx-cage.log ---\n' >&2; tail -120 /tmp/lnx-cage.log >&2; }
  [ -f /tmp/lnx-wayvnc.log ] && { printf '\n--- /tmp/lnx-wayvnc.log ---\n' >&2; tail -120 /tmp/lnx-wayvnc.log >&2; }
  [ -f /tmp/lnx-websockify.log ] && { printf '\n--- /tmp/lnx-websockify.log ---\n' >&2; tail -120 /tmp/lnx-websockify.log >&2; }
}

dedupe_apt_sources() {
  if [ -s /etc/apt/sources.list ] && [ -s /etc/apt/sources.list.d/ubuntu.sources ]; then
    log "disabling duplicate /etc/apt/sources.list; ubuntu.sources is present"
    sudo mv /etc/apt/sources.list /etc/apt/sources.list.lnx-disabled
  fi
}

configure_novnc_cursor() {
  [ -f /usr/share/novnc/app/ui.js ] || return 0

  log "configuring noVNC client cursor fallback"
  sudo sed -i "s/UI\\.initSetting('show_dot', false);/UI.initSetting('show_dot', true);/" /usr/share/novnc/app/ui.js
}

log "checking apt sources"
dedupe_apt_sources

set --
for package in ca-certificates curl snapd squashfs-tools cage wayvnc novnc websockify; do
  if ! dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q "install ok installed"; then
    set -- "$@" "$package"
  fi
done

if [ "$#" -gt 0 ]; then
  log "checking guest network"
  timeout 20s getent hosts ports.ubuntu.com >/dev/null || {
    log "cannot resolve ports.ubuntu.com from the guest"
    cat /etc/resolv.conf >&2 || true
    ip route >&2 || true
    exit 1
  }
  log "installing apt packages: $*"
  apt_get update
  apt_get install -y "$@"
else
  log "apt packages already installed"
fi

configure_novnc_cursor

log "starting snapd"
sudo systemctl reset-failed snapd.socket snapd.service || true
sudo systemctl enable --now snapd.socket
sudo systemctl start snapd.service || true

snap_version_output=""
snap_version_status=1
for _ in $(seq 1 120); do
  snap_version_output="$(timeout 15s snap version 2>&1)" && break
  snap_version_status=$?
  if [ "$snap_version_status" -ne 124 ] && printf '%s\n' "$snap_version_output" | grep -q 'panic:'; then
    printf '%s\n' "$snap_version_output" >&2
    sudo systemctl status --no-pager snapd.socket snapd.service >&2 || true
    exit "$snap_version_status"
  fi
  if systemctl is-failed --quiet snapd.socket || systemctl is-failed --quiet snapd.service; then
    printf '%s\n' "$snap_version_output" >&2
    sudo systemctl status --no-pager snapd.socket snapd.service >&2 || true
    exit 1
  fi
  if [ "$snap_version_status" -eq 0 ]; then
    break
  fi
  sleep 1
done
snap version >/dev/null
log "snapd is ready"

log "waiting for snap seed"
if ! sudo timeout "${LNX_BROWSER_SETUP_SNAP_SEED_TIMEOUT:-300s}" snap wait system seed.loaded; then
  snap changes >&2 || true
  exit 1
fi

if ! snap list chromium >/dev/null 2>&1; then
  log "installing snap Chromium"
  sudo snap install chromium
else
  log "snap Chromium already installed"
fi

chromium_version="$(/snap/bin/chromium --version 2>/dev/null)"
printf '%s\n' "$chromium_version" | grep -q Chromium
log "$chromium_version"

log "writing browser service"
sudo tee /usr/local/bin/lnx-browser-test >/dev/null <<'SH'
#!/bin/sh
set -eu
export XDG_RUNTIME_DIR=/run/user/0
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
export WAYLAND_DISPLAY=wayland-0

mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR" || true

pkill wayvnc 2>/dev/null || true
pkill websockify 2>/dev/null || true
pkill cage 2>/dev/null || true
pkill chromium 2>/dev/null || true
rm -f "$XDG_RUNTIME_DIR"/wayland-*.lock "$XDG_RUNTIME_DIR"/wayland-[0-9] \
  /tmp/lnx-cage.log /tmp/lnx-wayvnc.log /tmp/lnx-websockify.log

trap 'pkill wayvnc 2>/dev/null || true; pkill websockify 2>/dev/null || true; pkill cage 2>/dev/null || true' EXIT

cage -- /snap/bin/chromium \
  --no-sandbox \
  --disable-gpu \
  --disable-dev-shm-usage \
  --remote-debugging-address=0.0.0.0 \
  --remote-debugging-port=9222 \
  --user-data-dir=/tmp/lnx-browser-profile \
  --ozone-platform=wayland \
  --window-size=1280,800 \
  https://example.com >/tmp/lnx-cage.log 2>&1 &

for _ in $(seq 1 100); do
  [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
  sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || { tail -120 /tmp/lnx-cage.log; exit 1; }

printf 'enable_auth=false\n' >/tmp/lnx-wayvnc.ini
env XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  wayvnc -C /tmp/lnx-wayvnc.ini 127.0.0.1 5900 >/tmp/lnx-wayvnc.log 2>&1 &

for _ in $(seq 1 100); do
  ss -ltn sport = :5900 | grep -q 5900 && break
  sleep 0.1
done
ss -ltn sport = :5900 | grep -q 5900 || { tail -120 /tmp/lnx-wayvnc.log; tail -120 /tmp/lnx-cage.log; exit 1; }

websockify --web /usr/share/novnc 0.0.0.0:6080 127.0.0.1:5900 >/tmp/lnx-websockify.log 2>&1 &

for _ in $(seq 1 100); do
  ss -ltn sport = :6080 | grep -q 6080 && break
  sleep 0.1
done
ss -ltn sport = :6080 | grep -q 6080 || { tail -120 /tmp/lnx-websockify.log; exit 1; }

wait
SH

sudo chmod +x /usr/local/bin/lnx-browser-test

sudo tee /etc/systemd/system/lnx-browser-test.service >/dev/null <<'UNIT'
[Unit]
Description=lnx browser snapshot test
After=multi-user.target snapd.service

[Service]
Type=simple
ExecStart=/usr/local/bin/lnx-browser-test
Restart=on-failure

[Install]
WantedBy=multi-user.target
UNIT

log "starting browser service"
sudo systemctl daemon-reload
sudo systemctl enable lnx-browser-test.service
sudo systemctl restart lnx-browser-test.service

for _ in $(seq 1 60); do
  if curl -fsS http://127.0.0.1:6080/vnc.html 2>/dev/null | grep -i novnc >/dev/null \
    && curl -fsS http://127.0.0.1:9222/json/version 2>/dev/null | grep -i webSocketDebuggerUrl >/dev/null; then
    log "ready: noVNC http://127.0.0.1:6080/vnc.html"
    log "ready: noVNC .lnx https://p6080-default.lnx/vnc.html?show_dot=1"
    log "ready: CDP http://127.0.0.1:9222/json/version"
    log "ready: CDP .lnx https://p9222-default.lnx/json/version"
    exit 0
  fi
  sleep 1
done

show_failure_logs
exit 1
