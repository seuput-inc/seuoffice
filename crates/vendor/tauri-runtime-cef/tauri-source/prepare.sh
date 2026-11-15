#!/bin/sh
# Prepare the exact crates.io Tauri source with the runtime-builder accessor.
set -eu
: "${OMP_SCRATCH_DIR:?Set to an approved scratch directory}"
: "${TAURI_CRATE_ARCHIVE:?Set to the tauri-3.0.0-alpha.6.crate archive}"
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum < "$1" | cut -d ' ' -f 1
  else shasum -a 256 < "$1" | cut -d ' ' -f 1
  fi
}
[ "$(sha256 "$TAURI_CRATE_ARCHIVE")" = "4b0cad7bd420aa41bdf7beab9a95e02d83c4cab1cb6f7b94fe4fd1e0c605feaa" ] || { echo 'Tauri archive checksum mismatch' >&2; exit 1; }
patch_file="$here/0001-window-builder-runtime-access.patch"
[ "$(sha256 "$patch_file")" = "c8f75c9eb064541ea0e5f5d60d87b2c3dda2f53ec566a4509bd71e056927166e" ] || { echo 'Tauri patch checksum mismatch' >&2; exit 1; }
work=$(mktemp -d "$OMP_SCRATCH_DIR/tauri-source.XXXXXXXX")
trap 'status=$?; if [ "$status" -ne 0 ]; then echo "Failed Tauri stage retained at $work" >&2; fi' EXIT HUP INT TERM
tar -xzf - -C "$work" < "$TAURI_CRATE_ARCHIVE"
source_dir="$work/tauri-3.0.0-alpha.6"
[ -f "$source_dir/Cargo.toml" ] || { echo 'Unexpected Tauri archive layout' >&2; exit 1; }
(cd "$source_dir" && patch -t -N -p1 < "$patch_file")
printf '%s\n' "$source_dir"
