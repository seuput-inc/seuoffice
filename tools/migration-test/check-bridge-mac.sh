#!/usr/bin/env bash
set -euo pipefail

app=$1 zip=$2 dmg=$3
work=$(mktemp -d "${RUNNER_TEMP:?}/bridge-mac.XXXXXXXX")
mount="$work/dmg"
mounted=false
cleanup() {
  if [ "$mounted" = true ]; then hdiutil detach "$mount" || return 1; fi
  rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/zip" "$mount"
ditto -x -k "$zip" "$work/zip"
hdiutil attach -readonly -nobrowse -mountpoint "$mount" "$dmg"
mounted=true
copies=("$work/zip/SeuOffice.app" "$mount/SeuOffice.app")
crates/shell/ci/mac-check-signature.sh "$app" "${copies[@]}"
# Both containers must carry the exact signed code checked in the unpacked build.
cdhash() { codesign -dv --verbose=4 "$1" 2>&1 | sed -n 's/^CDHash=//p'; }
expected=$(cdhash "$app")
[[ "$expected" =~ ^[a-fA-F0-9]{40}$ ]]
for copy in "${copies[@]}"; do
  test "$(cdhash "$copy")" = "$expected"
done
