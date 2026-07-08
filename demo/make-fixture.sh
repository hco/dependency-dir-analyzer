#!/usr/bin/env bash
#
# Generate a self-contained, fake "projects" tree for the VHS demo.
#
# The tree is NOT committed (see .gitignore) — regenerate it with
# `mise run demo-fixture`, or let `mise run demo` build it for you.
#
# Sizes are faked with sparse files (`mkfile -n`, falling back to `truncate`),
# so a "1.2 GB" node_modules costs a few KB of real disk. dda sizes by file
# length (`metadata.len()`), so the reported numbers still look real. Each
# subtree's mtime is back-dated so the "Modified" column shows a realistic
# spread of ages.
#
# macOS/BSD `date -v` and `mkfile` are the happy path; GNU `date -d` and
# `truncate` are used as fallbacks on Linux.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/fixture"
rm -rf "$ROOT"
mkdir -p "$ROOT"

# make_sparse <path> <size> — create a file that *reports* <size> bytes.
make_sparse() {
  local path="$1" size="$2"
  if command -v mkfile >/dev/null 2>&1; then
    mkfile -n "$size" "$path"
  else
    # GNU truncate wants an upper-case unit suffix.
    truncate -s "${size^^}" "$path"
  fi
}

# pad_files <dir> <count> — drop <count> tiny files so the "Files" column is plausible.
pad_files() {
  local dir="$1" count="$2" i
  mkdir -p "$dir/pkg"
  for ((i = 0; i < count; i++)); do
    : >"$dir/pkg/f$i"
  done
}

# age <dir> <offset> — back-date the whole subtree so its newest mtime is <offset> ago.
# <offset> is a BSD `date -v` adjustment like "-2d", "-5m", "-1y".
age() {
  local dir="$1" offset="$2" stamp
  if stamp="$(date -v"$offset" +%Y%m%d%H%M.%S 2>/dev/null)"; then
    :
  else
    # GNU date: translate "-5m" (months) / "-3w" / "-2d" / "-1y" into words.
    local n="${offset:1:${#offset}-2}" unit="${offset: -1}"
    case "$unit" in
    d) unit=days ;; w) unit=weeks ;; m) unit=months ;; y) unit=years ;;
    esac
    stamp="$(date -d "$n $unit ago" +%Y%m%d%H%M.%S)"
  fi
  find "$dir" -exec touch -t "$stamp" {} +
}

# entry <relpath> <size> <padfiles> <age-offset>
entry() {
  local rel="$1" size="$2" pad="$3" off="$4"
  local dir="$ROOT/$rel"
  mkdir -p "$dir"
  make_sparse "$dir/blob.bin" "$size"
  pad_files "$dir" "$pad"
  age "$dir" "$off"
}

entry web-app/node_modules  1200m 900 -2d
entry mobile/Pods            260m  400 -2m
entry cli-tool/target        420m  300 -5m
entry ml/.venv               180m  250 -1y
entry docs-site/.next         92m  180 -4d
entry infra/.terraform        68m  120 -6d
entry api/vendor              36m  140 -3w
entry service/__pycache__     12m   60 -9m

echo "Fixture ready at $ROOT"
