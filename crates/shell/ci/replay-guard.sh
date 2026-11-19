#!/bin/bash
# Tells a shell executable built with the `e2e-replay` feature from one built without it.
#
# The feature compiles the replay markers of the native-only IPC record
# (crates/shell/src/ipc/native_record/replay.rs): the real-app test build has them, an executable
# that ships must not. The marker's key is in the executable through that module alone, so its
# bytes decide. The name of the record file is in every build of the shell: it shows that the file
# is the shell and that its texts can be read at all.
#
#   crates/shell/ci/replay-guard.sh absent <executable>    an executable that ships
#   crates/shell/ci/replay-guard.sh present <executable>   the e2e build: the check of this guard
set -euo pipefail

marker=e2eReplay
control=native-ipc.jsonl
mode="${1:-}"
exe="${2:-}"
case "$mode" in
  absent|present) ;;
  *) echo "Usage: $0 {absent|present} <executable>" >&2; exit 2 ;;
esac
if [ ! -f "$exe" ]; then
  echo "replay-guard: there is no executable at '$exe'" >&2
  exit 1
fi
# grep ends with 1 when it counts no line; anything but a number below is a failed read
count() { LC_ALL=C grep -a -c -F -e "$1" "$exe" || true; }
found=$(count "$marker")
known=$(count "$control")
case "$found:$known" in
  *[!0-9:]*|:*|*:) echo "replay-guard: cannot read '$exe'" >&2; exit 1 ;;
esac
if [ "$known" = 0 ]; then
  echo "replay-guard: '$exe' does not hold '$control': not the shell, or its texts cannot be read" >&2
  exit 1
fi
if [ "$mode" = absent ] && [ "$found" != 0 ]; then
  echo "replay-guard: '$exe' holds '$marker': built with the e2e-replay feature, it must not ship" >&2
  exit 1
fi
if [ "$mode" = present ] && [ "$found" = 0 ]; then
  echo "replay-guard: '$exe' does not hold '$marker': the e2e build lacks the e2e-replay feature, or this guard looks for the wrong bytes" >&2
  exit 1
fi
echo "replay-guard: $mode ok ('$marker' on $found lines of $exe)"
