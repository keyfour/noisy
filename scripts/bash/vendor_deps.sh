#!/usr/bin/env bash
# Vendored dependency acquisition (specification §7): every outside file
# enters the repository through this pinned, verified, idempotent script —
# never by hand. Runs offline when the pinned file is already correct.
#
# Usage: scripts/bash/vendor_deps.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

# name | version | target | url | sha256
VENDOR=(
  "json.hpp|3.12.0|sdk/cpp/nlohmann/json.hpp|https://github.com/nlohmann/json/releases/download/v3.12.0/json.hpp|aaf127c04cb31c406e5b04a63f1ae89369fccde6d8fa7cdda1ed4f32dfc5de63"
)

digest() { sha256sum "$1" | cut -d' ' -f1; }

for entry in "${VENDOR[@]}"; do
  IFS='|' read -r name version target url want <<<"$entry"
  if [ -f "$target" ] && [ "$(digest "$target")" = "$want" ]; then
    echo "vendor: $name $version up to date at $target"
    continue
  fi
  echo "vendor: fetching $name $version -> $target"
  tmp="$target.tmp"
  mkdir -p "$(dirname "$target")"
  curl --fail --silent --show-error --location "$url" -o "$tmp"
  got="$(digest "$tmp")"
  if [ "$got" != "$want" ]; then
    echo "vendor: sha256 mismatch for $name: got $got want $want" >&2
    rm -f "$tmp"
    exit 1
  fi
  mv "$tmp" "$target"
  echo "vendor: $name $version installed (sha256 verified)"
done
