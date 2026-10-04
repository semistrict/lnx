#!/bin/sh
# Installs the demo desktop's own files: the session script, the cua wrapper,
# the service, the demo page and the Openbox config. Runs as root inside the
# VM. provision.sh runs it once; demo.ts runs it before every recording so
# changes here need no re-provisioning.
#
# Usage: install-desktop.sh <dir containing these files>
set -eu

SRC="$1"
DESKTOP_USER=ubuntu

install -m 0755 "$SRC/desktop.sh" /usr/local/bin/cua-desktop
install -m 0755 "$SRC/cua" /usr/local/bin/cua
install -m 0644 "$SRC/cua-desktop.service" /etc/systemd/system/cua-desktop.service
install -d -m 0755 /opt/cua-demo/www
install -m 0644 "$SRC/www/index.html" /opt/cua-demo/www/index.html
install -d -m 0755 -o "$DESKTOP_USER" -g "$DESKTOP_USER" \
  "/home/$DESKTOP_USER/.config" "/home/$DESKTOP_USER/.config/openbox"
install -m 0644 -o "$DESKTOP_USER" -g "$DESKTOP_USER" "$SRC/openbox-rc.xml" \
  "/home/$DESKTOP_USER/.config/openbox/rc.xml"
install -d -m 0700 -o "$DESKTOP_USER" -g "$DESKTOP_USER" /run/cua-desktop
printf 'd /run/cua-desktop 0700 %s %s -\n' "$DESKTOP_USER" "$DESKTOP_USER" >/etc/tmpfiles.d/cua-desktop.conf
systemctl daemon-reload
systemctl enable cua-desktop.service
