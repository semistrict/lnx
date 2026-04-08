#!/usr/bin/env bash
set -euo pipefail

type_text() {
  local text="$1"
  local delay="${2:-0.028}"
  local i char
  for ((i = 0; i < ${#text}; i++)); do
    char="${text:i:1}"
    printf '%s' "$char"
    sleep "$delay"
  done
}

run_cmd() {
  local cmd="$1"
  printf '\033[1;36m$\033[0m '
  type_text "$cmd"
  printf '\n'
}

say() {
  local text="$1"
  local delay="${2:-0}"
  printf '%b\n' "$text"
  if [[ "$delay" != "0" ]]; then
    sleep "$delay"
  fi
}

pause() {
  sleep "${1:-0.6}"
}

if ! clear 2>/dev/null; then
  printf '\033[H\033[2J'
fi

say $'\033[1;37mlnx ingress demo\033[0m'
say $'\033[2mStart from a clean Mac terminal, set up a VM, run a dev server, curl it via .lnx.\033[0m'
pause 1

run_cmd 'lnx init'
pause 0.5
say $'\033[32minstalled ~/.lnx/vmlinuz\033[0m'
say $'\033[32minstalled ~/.lnx/instances/default/rootfs.ext4\033[0m'
pause 1

run_cmd 'lnx instance create dev'
pause 0.4
say $'\033[32mcreated instance "dev"\033[0m'
pause 1

run_cmd 'mkdir web-demo && cd web-demo'
pause 0.8

run_cmd 'npm create vite@latest . -- --template react'
pause 0.5
say $'\033[32m◇  Scaffolding project in ./web-demo...\033[0m'
say $'\033[32m└  Done. Now run npm install and npm run dev\033[0m'
pause 1

run_cmd 'npm install'
pause 0.5
say $'\033[2madded 153 packages in 4s\033[0m'
pause 1

run_cmd "lnx --instance dev sh -lc 'cd /work/web-demo && npm run dev -- --host 0.0.0.0 --port 5173'"
pause 0.8
say $'\033[2m> web-demo@0.0.0 dev\033[0m'
say $'\033[2m> vite --host 0.0.0.0 --port 5173\033[0m'
pause 0.8
say $'\033[32mVITE v7.1.7 ready in 421 ms\033[0m'
say $'\033[2m➜  Local:   http://localhost:5173/\033[0m'
say $'\033[2m➜  Network: http://192.168.64.2:5173/\033[0m'
pause 1.2

run_cmd 'lnx ingress enable'
pause 0.4
say $'\033[32mingress enabled for .lnx\033[0m'
pause 1

run_cmd "curl -s http://p5173.dev.lnx/ | rg '<title>'"
pause 0.5
say $'\033[32m<title>Vite + React</title>\033[0m'
pause 1

run_cmd 'open http://p5173.dev.lnx/'
pause 0.5
say $'\033[32mbrowser opened\033[0m'
pause 1
