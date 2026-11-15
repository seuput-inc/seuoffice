#!/usr/bin/env bash
# Hosted-only source preparation; preserve the pinned crate and patch identities.
set -euo pipefail
[[ "${GITHUB_ACTIONS:-}" == true ]] || { echo 'Tauri source preparation requires the hosted runner' >&2; exit 1; }
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
root="$(cd "$(dirname "$0")/../../.." && pwd)"
runner_temp="$RUNNER_TEMP"
if command -v cygpath >/dev/null 2>&1; then
  runner_temp="$(cygpath -u "$runner_temp")"
fi
stage="$(mktemp -d "$runner_temp/seuoffice-tauri.XXXXXXXX")"
archive="$stage/tauri-3.0.0-alpha.6.crate"
curl --fail --location --proto '=https' --tlsv1.2 \
  https://static.crates.io/crates/tauri/tauri-3.0.0-alpha.6.crate --output "$archive"
source_dir="$(OMP_SCRATCH_DIR="$stage" TAURI_CRATE_ARCHIVE="$archive" \
  sh "$root/crates/vendor/tauri-runtime-cef/tauri-source/prepare.sh" | tail -n 1)"
[[ -f "$source_dir/Cargo.toml" ]] || { echo 'Prepared Tauri source is missing' >&2; exit 1; }
if command -v cygpath >/dev/null 2>&1; then
  source_dir="$(cygpath -m "$source_dir")"
  root="$(cygpath -m "$root")"
fi
python3 - "$root" "$source_dir" <<'PY'
import json
from pathlib import Path
import sys
root, source = map(Path, sys.argv[1:])
config = root / 'crates/shell/.cargo/config.toml'
config.parent.mkdir(exist_ok=True)
with config.open('x') as output:
    output.write('[patch.crates-io.tauri]\npath = ' + json.dumps(str(source)) + '\n')
print('Cargo Tauri override: ' + str(source))
PY
