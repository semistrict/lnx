#!/bin/sh
# Guest provisioning for the cua fork demo. Runs as root inside an lnx VM.
#
# Installs an X11 desktop (Xvfb + Openbox), Chromium (the open-source build
# from the xtradeb/apps PPA, at the root-owned path cua-driver accepts),
# cua-driver (MIT, https://github.com/trycua/cua) and ffmpeg, then installs
# the cua-desktop service that runs them for the `ubuntu` user
# (install-desktop.sh).
#
# Usage: provision.sh <dir containing this demo's files>
set -eu

SRC="$1"
CUA_DRIVER_VERSION="${CUA_DRIVER_VERSION:-0.33.1}"
XTRADEB_KEY=5301FA4FD93244FBC6F6149982BB6851C64F6880
DESKTOP_USER=ubuntu

export DEBIAN_FRONTEND=noninteractive

log() { printf 'provision: %s\n' "$*"; }

apt_get() {
  apt-get -o Acquire::Retries=3 -o Dpkg::Lock::Timeout=120 "$@"
}

log "apt packages"
apt_get update
apt_get install -y --no-install-recommends \
  xvfb openbox x11-utils x11-xserver-utils xauth dbus-x11 at-spi2-core \
  fonts-dejavu-core fonts-noto-color-emoji ffmpeg ca-certificates curl gpg \
  python3

log "chromium from xtradeb/apps (pinned key $XTRADEB_KEY)"
install -d -m 0755 /etc/apt/keyrings
GNUPGHOME="$(mktemp -d)"
export GNUPGHOME
key_tmp="$(mktemp)"
curl -fsSL "https://keyserver.ubuntu.com/pks/lookup?op=get&search=0x$XTRADEB_KEY" >"$key_tmp"
gpg --dearmor <"$key_tmp" >/etc/apt/keyrings/xtradeb-apps.gpg
rm -f "$key_tmp"
gpg --show-keys --with-colons /etc/apt/keyrings/xtradeb-apps.gpg | grep -q "^fpr:::::::::$XTRADEB_KEY:"
rm -rf "$GNUPGHOME"
unset GNUPGHOME
. /etc/os-release
echo "deb [signed-by=/etc/apt/keyrings/xtradeb-apps.gpg] https://ppa.launchpadcontent.net/xtradeb/apps/ubuntu $VERSION_CODENAME main" \
  >/etc/apt/sources.list.d/xtradeb-apps.list
# Only Chromium may come from the PPA.
printf 'Package: *\nPin: origin ppa.launchpadcontent.net\nPin-Priority: -1\n\nPackage: chromium chromium-common chromium-sandbox\nPin: origin ppa.launchpadcontent.net\nPin-Priority: 1001\n' \
  >/etc/apt/preferences.d/xtradeb-chromium
apt_get update
apt_get install -y --no-install-recommends chromium chromium-common
test -x /usr/lib/chromium/chromium
install -d -m 0755 /etc/chromium/policies/managed
cat >/etc/chromium/policies/managed/demo.json <<'JSON'
{"BrowserSignin": 0, "SyncDisabled": true, "PasswordManagerEnabled": false,
 "PromotionsEnabled": false, "DefaultBrowserSettingEnabled": false,
 "MetricsReportingEnabled": false, "PrivacySandboxPromptEnabled": false,
 "TranslateEnabled": false}
JSON

log "cua-driver $CUA_DRIVER_VERSION"
work="$(mktemp -d)"
base="https://github.com/trycua/cua/releases/download/cua-driver-rs-v$CUA_DRIVER_VERSION"
dist="cua-driver-rs-$CUA_DRIVER_VERSION-linux-arm64"
curl -fsSL -o "$work/$dist.tar.gz" "$base/$dist.tar.gz"
curl -fsSL -o "$work/checksums.txt" "$base/checksums.txt"
(cd "$work" && grep " $dist.tar.gz\$" checksums.txt | sha256sum -c -)
tar -xzf "$work/$dist.tar.gz" -C "$work"
install -m 0755 "$work/$dist/cua-driver" /usr/local/bin/cua-driver
install -d -m 0755 /usr/local/share/doc/cua-driver
install -m 0644 "$work/$dist/LICENSE" "$work/$dist/THIRD_PARTY_NOTICES.md" /usr/local/share/doc/cua-driver/
rm -rf "$work"
cua-driver --version

log "desktop"
sh "$SRC/install-desktop.sh" "$SRC"
runuser -u "$DESKTOP_USER" -- env HOME="/home/$DESKTOP_USER" cua-driver telemetry disable

log "done"
