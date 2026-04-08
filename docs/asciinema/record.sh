#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

asciinema rec \
  --overwrite \
  --output-format asciicast-v2 \
  --headless \
  --idle-time-limit 0.8 \
  --window-size 108x32 \
  --title "lnx ingress demo" \
  --command "env TERM=xterm-256color bash ./demo.sh" \
  ingress-demo.cast

printf 'wrote %s\n' "docs/asciinema/ingress-demo.cast"
