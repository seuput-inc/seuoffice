#!/bin/sh
# Check the private patches against pristine, pinned source trees without building CEF.
set -eu
: "${CHROMIUM_SOURCE_ROOT:?Set to the pristine pinned Chromium source root}"
: "${CEF_SOURCE_ROOT:?Set to the pristine pinned CEF source root}"
: "${OMP_SCRATCH_DIR:?Set to an approved scratch directory}"
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d "$OMP_SCRATCH_DIR/cef-patch-check.XXXXXXXX")
trap 'status=$?; if [ "$status" -eq 0 ]; then rm -rf "$work"; else printf "Failed source stage retained at %s\n" "$work" >&2; fi' EXIT HUP INT TERM
mkdir -p "$work/chromium" "$work/cef"

stage() {
  source_root=$1
  destination=$2
  shift 2
  for patch_file do
    awk '$1 == "---" && $2 ~ /^a\// { print substr($2, 3) }
         $1 == "+++" && $2 ~ /^b\// { print substr($2, 3) }' "$patch_file" |
    while IFS= read -r path; do
      mkdir -p "$destination/$(dirname "$path")"
      if [ -f "$source_root/$path" ] && [ ! -f "$destination/$path" ]; then
        cp "$source_root/$path" "$destination/$path"
      fi
    done
  done
}

chromium_patches="
0002-chromium-pending-document.patch
0006-before-navigation-revoke.patch
0011-chromium-approved-history.patch
0013-chromium-context-prethaw.patch
0015-chromium-popup-origin.patch
0017-chromium-background-throttling.patch
0020-chromium-resource-document.patch
0024-chromium-process-owner-admission.patch
0027-chromium-context-process-loss.patch
0030-chromium-observed-document-retirement.patch"
cef_patches="
0001-web-contents-focus.patch
0003-cef-context-events.patch
0004-cef-early-context-channel.patch
0005-approved-navigation.patch
0007-native-promise-eval.patch
0008-native-context-evaluation-channel.patch
0009-native-context-traffic.patch
0010-native-v8-bridge.patch
0012-cef-approved-history.patch
0014-cef-retained-bridge.patch
0016-cef-popup-origin.patch
0018-cef-background-throttling.patch
0019-native-context-document-ready.patch
0021-cef-resource-origin.patch
0022-cef-native-renderer-process.patch
0023-cef-registration-incarnation.patch
0025-cef-process-owner-admission.patch
0026-cef-observed-navigation.patch
0028-cef-context-process-loss.patch
0029-cef-observed-document-lifetime.patch
0031-cef-observed-document-retirement.patch
0032-cef-observed-result-gate.patch
0033-cef-observed-cdp-correlation.patch"
python3 - "$here" <<'PY'
import hashlib
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
for line in (root / "patches.sha256").read_text().splitlines():
    expected, name = line.split("  ", 1)
    actual = hashlib.sha256((root / name).read_bytes()).hexdigest()
    if actual != expected:
        raise SystemExit(f"Patch checksum mismatch: {name}")
PY
set --
for name in $chromium_patches; do set -- "$@" "$here/$name"; done
stage "$CHROMIUM_SOURCE_ROOT" "$work/chromium" "$@"
for patch_file do
  patch -t -N -d "$work/chromium" -p1 < "$patch_file"
done
set --
for name in $cef_patches; do set -- "$@" "$here/$name"; done
stage "$CEF_SOURCE_ROOT" "$work/cef" "$@"
for patch_file do
  patch -t -N -d "$work/cef" -p1 < "$patch_file"
done
printf '%s\n' 'Pinned source patch stack applied; generated ABI, compilation and runtime are not verified.'
