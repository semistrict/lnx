#!/usr/bin/env bash
# Model-check every TLA+ spec under specs/tla/ with TLC.
#
# Convention (see specs/tla/README.md):
#   specs/tla/<Dir>/<Module>.tla             a module
#   specs/tla/<Dir>/<Module>.cfg             a TLC config for <Module>
#   specs/tla/<Dir>/<Module>.<Variant>.cfg   another TLC config for <Module>
# Every .cfg starts with an expectation line:
#   \* EXPECT: pass                 TLC must finish with no error
#   \* EXPECT: violation <Name>     TLC must report "Invariant <Name> is violated"
# Current-design configs that reproduce a known bug use "violation"; target
# design configs use "pass". The script exits non-zero if any config does not
# meet its expectation: a target spec fails, or a current-design spec stops
# reproducing its bug (fix the code, then flip or delete that config).
#
# Usage: scripts/tla/check.sh [filter]   (filter = substring of the cfg path)
# Set TLA_KEEP_LOGS=<dir> to keep every TLC log (counterexample traces).
set -euo pipefail

TLA_VERSION="1.7.4"
TLA_SHA256="936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
TLA_URL="https://github.com/tlaplus/tlaplus/releases/download/v${TLA_VERSION}/tla2tools.jar"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/lnx"
jar="${cache_dir}/tla2tools-${TLA_VERSION}.jar"

java_bin="${JAVA:-}"
if [[ -z "${java_bin}" ]]; then
  if [[ -x /opt/homebrew/opt/openjdk/bin/java ]]; then
    java_bin=/opt/homebrew/opt/openjdk/bin/java
  else
    java_bin="$(command -v java || true)"
  fi
fi
if [[ -z "${java_bin}" ]]; then
  echo "error: java not found (set JAVA=/path/to/java)" >&2
  exit 2
fi

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    sha256sum "$1" | cut -d' ' -f1
  fi
}

if [[ ! -f "${jar}" ]] || [[ "$(sha256_of "${jar}")" != "${TLA_SHA256}" ]]; then
  mkdir -p "${cache_dir}"
  tmp_jar="$(mktemp "${cache_dir}/tla2tools.XXXXXX")"
  echo "downloading tla2tools ${TLA_VERSION} -> ${jar}"
  curl -fsSL -o "${tmp_jar}" "${TLA_URL}"
  actual="$(sha256_of "${tmp_jar}")"
  if [[ "${actual}" != "${TLA_SHA256}" ]]; then
    rm -f "${tmp_jar}"
    echo "error: tla2tools.jar sha256 mismatch: expected ${TLA_SHA256}, got ${actual}" >&2
    exit 2
  fi
  mv "${tmp_jar}" "${jar}"
fi

filter="${1:-}"
work_root="$(mktemp -d "${TMPDIR:-/tmp}/lnx-tlc.XXXXXX")"
trap 'rm -rf "${work_root}"' EXIT

failures=0
checked=0
# The config list is read from fd 3 so that nothing run inside the loop can
# consume it from stdin.
while IFS= read -r -u 3 cfg; do
  if [[ -n "${filter}" && "${cfg}" != *"${filter}"* ]]; then
    continue
  fi
  dir="$(dirname "${cfg}")"
  base="$(basename "${cfg}" .cfg)"
  module="${base%%.*}"
  rel="${cfg#"${repo_root}/"}"
  expect="$(sed -n 's/^\\\* EXPECT: *//p' "${cfg}" </dev/null | sed -n 1p)"
  if [[ -z "${expect}" ]]; then
    echo "FAIL ${rel}: missing '\\* EXPECT:' line"
    failures=$((failures + 1))
    continue
  fi
  log="${work_root}/${base}.log"
  meta="${work_root}/${base}.meta"
  mkdir -p "${meta}"
  start=$(date +%s)
  set +e
  (cd "${dir}" && "${java_bin}" -XX:+UseParallelGC -cp "${jar}" tlc2.TLC \
      -config "$(basename "${cfg}")" -metadir "${meta}" -workers auto \
      -deadlock -cleanup "${module}.tla") </dev/null >"${log}" 2>&1
  status=$?
  set -e
  elapsed=$(( $(date +%s) - start ))
  checked=$((checked + 1))
  output="$(cat "${log}")"
  states="$(sed -n 's/^\([0-9,]* states generated, [0-9,]* distinct states found\).*/\1/p' "${log}" </dev/null | sed -n '$p')"
  case "${expect}" in
    pass)
      if [[ ${status} -eq 0 && "${output}" == *"No error has been found"* ]]; then
        echo "ok   ${rel} (expected pass; ${states}; ${elapsed}s)"
      else
        echo "FAIL ${rel}: expected pass, TLC exit ${status}"
        sed -n '/Error:/,$p' "${log}" </dev/null | sed -n '1,80p'
        failures=$((failures + 1))
      fi
      ;;
    violation\ *)
      inv="${expect#violation }"
      if [[ "${output}" == *"Invariant ${inv} is violated"* ]]; then
        trace_len="$(sed -n 's/^State \([0-9]*\):.*/\1/p' "${log}" </dev/null | sed -n '$p')"
        echo "ok   ${rel} (expected violation of ${inv} reproduced; trace ${trace_len} states; ${elapsed}s)"
      else
        echo "FAIL ${rel}: expected violation of ${inv}, TLC exit ${status}"
        sed -n '/Error:/,$p;/No error has been found/p' "${log}" </dev/null | sed -n '1,40p'
        failures=$((failures + 1))
      fi
      ;;
    *)
      echo "FAIL ${rel}: unknown expectation '${expect}'"
      failures=$((failures + 1))
      ;;
  esac
  if [[ -n "${TLA_KEEP_LOGS:-}" ]]; then
    mkdir -p "${TLA_KEEP_LOGS}"
    cp "${log}" "${TLA_KEEP_LOGS}/${base}.log"
  fi
done 3< <(find "${repo_root}/specs/tla" -name '*.cfg' | sort)

if [[ ${checked} -eq 0 ]]; then
  echo "error: no TLC configs matched" >&2
  exit 2
fi
if [[ ${failures} -ne 0 ]]; then
  echo "${failures} of ${checked} TLC configs did not meet their expectation" >&2
  exit 1
fi
echo "all ${checked} TLC configs met their expectation"
