#!/bin/bash
# Run the real shell's Cargo check, clippy, or tests on macOS or Windows.
# Build the preloads first: src/preload.rs includes them.
#
#   crates/shell/ci/check-local.sh check
#   crates/shell/ci/check-local.sh clippy --no-deps --all-targets -- -D warnings
#   crates/shell/ci/check-local.sh test
set -euo pipefail

case "$(uname -s)" in
  Darwin|MINGW*|MSYS*) ;;
  *) echo "Shell checks require macOS or Windows" >&2; exit 1 ;;
esac
case "${1:-}" in
  check|clippy|test) command="$1"; shift ;;
  *) echo "Usage: $0 {check|clippy|test} [cargo arguments...]" >&2; exit 2 ;;
esac
shell_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$shell_dir"
exec cargo "$command" --locked "$@"
