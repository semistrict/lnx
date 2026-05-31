#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

mkdir -p out

agg \
  --theme asciinema \
  --font-size 18 \
  --speed 1.15 \
  --idle-time-limit 0.8 \
  ingress-demo.cast \
  out/ingress-demo.gif

printf 'wrote %s\n' "docs/asciinema/out/ingress-demo.gif"
