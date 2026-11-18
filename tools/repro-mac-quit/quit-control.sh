#!/usr/bin/env bash
# The quit control of tools/migration-test/mac-runner.sh on a freshly built app:
# launch it with an isolated profile, wait, ask it to quit the way the Dock does,
# and record whether it exits. On a hang it also captures what the process was
# doing and whether a second quit request ends it.
#
#   quit-control.sh <SeuOffice.app> <out dir>
set -uo pipefail
app=$1 out=$2
exe="$app/Contents/MacOS/SeuOffice"
port=9229
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"
hang=
now() { perl -MTime::HiRes=time -e 'printf "%.2f\n", time'; }
result() { printf '%s\n' "$1" | tee -a "$out/result.txt"; }
ask_quit() { osascript -e "tell application \"$app\" to quit" >>"$out/osascript.log" 2>&1 || true; }
node "$here/probe.mjs" make-docx "$out/migration-test.docx"

diagnose() {
  local d="$out/diag-$1"
  mkdir -p "$d"
  { ps -axo pid,ppid,etime,stat,command | grep -i '[S]euOffice'; lsof -nP -iTCP:$port -sTCP:LISTEN; } >"$d/ps.txt" 2>&1 || true
  curl -sS --max-time 5 "http://127.0.0.1:$port/json/list" >"$d/json-list.txt" 2>&1 || true
  for p in $(pgrep -f "$exe"); do sample "$p" 3 -file "$d/sample-$p.txt" >/dev/null 2>&1 || true; done
  screencapture -x "$d/screen.png" 2>/dev/null || true
}

# quit_control <label> <seconds to wait before asking> <app args...>
quit_control() {
  local label=$1 warm=$2; shift 2
  local ud
  ud=$(mktemp -d "${RUNNER_TEMP:-/tmp}/seuoffice-ud-$label.XXXXXX")
  export SEUOFFICE_USER_DATA="$ud" SEUOFFICE_QUIT_TRACE=1 SEUOFFICE_QUIT_TRACE_FILE="$out/trace-$label.log"
  : >"$SEUOFFICE_QUIT_TRACE_FILE"
  "$exe" "$@" >"$out/stdout-$label.log" 2>&1 &
  local pid=$! started finished
  sleep "$warm"
  if ! kill -0 "$pid" 2>/dev/null; then
    result "quit-control $label: the app was not running before the quit request"
    hang=1
    return
  fi
  started=$(now)
  ask_quit
  for _ in $(seq 90); do
    if ! kill -0 "$pid" 2>/dev/null; then
      finished=$(now)
      result "quit-control $label: quit in $(perl -e "printf '%.1f', $finished - $started") s"
      return
    fi
    sleep 0.5
  done
  hang=1
  diagnose "$label"
  result "quit-control $label: still running 45 s after quit"
  started=$(now)
  ask_quit
  for _ in $(seq 30); do
    if ! kill -0 "$pid" 2>/dev/null; then
      finished=$(now)
      result "quit-control $label: a second quit request ended it in $(perl -e "printf '%.1f', $finished - $started") s"
      return
    fi
    sleep 0.5
  done
  result "quit-control $label: a second quit request did not end it either"
  kill -9 "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

quit_control plain 20 --use-mock-keychain
quit_control doc 20 --use-mock-keychain "$out/migration-test.docx"
quit_control port-doc 20 --remote-debugging-port=$port --use-mock-keychain "$out/migration-test.docx"
[ -z "$hang" ] || exit 1
result passed
