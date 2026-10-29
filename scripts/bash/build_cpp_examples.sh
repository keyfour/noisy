#!/usr/bin/env bash
# The one C++ build path (specification §7 repeatability rule): every C++
# artifact — CI's warnings-as-errors stage and the cargo e2e fixtures —
# compiles through this script, with the flags pinned in one place. Run
# scripts/bash/vendor_deps.sh first (no-op when the pinned file is present).
#
# Usage: scripts/bash/build_cpp_examples.sh
#   CXX (default `c++`) and BUILD_DIR (default `target/cpp`) override the
#   compiler and output directory; the examples engine.toml never needs it.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/bash/vendor_deps.sh >/dev/null

CXX=${CXX:-c++}
BUILD_DIR=${BUILD_DIR:-target/cpp}

# -std=c++20 (atomic_ref), -Wall -Wextra -Werror (CI builds examples with
# warnings as errors), -fPIC -shared (the single plugin format).
FLAGS=(-std=c++20 -shared -fPIC -Wall -Wextra -Werror -Isdk/cpp)

# module name | source dir | source file
EXAMPLES=(
  "mysensor|examples/cpp_module|sensor.cpp"
  "myapp|examples/cpp_app|app.cpp"
)

mkdir -p "$BUILD_DIR"
for entry in "${EXAMPLES[@]}"; do
  IFS='|' read -r module dir source <<<"$entry"
  # shellcheck disable=SC2086
  "$CXX" "${FLAGS[@]}" "$dir/$source" -o "$BUILD_DIR/lib$module.so"
  echo "built: $BUILD_DIR/lib$module.so"
done
