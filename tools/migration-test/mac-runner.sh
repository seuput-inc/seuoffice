#!/usr/bin/env bash
# The migration acceptance test of decision 0029 on a macOS runner: the path
# of win-desktop.ps1 without the faults. Install the Electron release FROM in
# /Applications, open a mock document, seed data, point the updater at a
# local feed offering TO_ZIP, update through the app's dialog (mac-update.ts
# stages and swaps the bundle), and check the updated app shows the data.
#
#   mac-runner.sh <from tag> <to zip> <to version> <out dir> <next zip> <next version> [to receipt] [next receipt]
#
# The bundle is signed again ad hoc after app-update.yml is rewritten; the
# AI key is seeded after that, so the keychain item belongs to the signed
# app (decision 0029 (b): a new signature asks once, as every update does).
set -euo pipefail
from=$1 to_zip=$2 to_version=$3 out=$4 next_zip=$5 next_version=$6
to_receipt=${7:-} next_receipt=${8:-}
[ "${GITHUB_ACTIONS:-}" = true ] && [ "${RUNNER_ENVIRONMENT:-}" = github-hosted ] && [ -n "${RUNNER_TEMP:-}" ] || { echo "Migration requires a GitHub-hosted runner and RUNNER_TEMP" >&2; exit 1; }
ud="$HOME/Library/Application Support/seuoffice"
export SEUOFFICE_USER_DATA="$ud"
here=$(cd "$(dirname "$0")" && pwd)
app=/Applications/SeuOffice.app
port=9229
mkdir -p "$out/from"
[ -z "$to_receipt" ] || cp "$to_receipt" "$out/to-receipt.json"
[ -z "$next_receipt" ] || cp "$next_receipt" "$out/next-receipt.json"
result() { printf '%s\n' "$1" >>"$out/result.txt"; echo "== $1"; }
if ! node - "$ud" <<'NODE'
const fs = require('node:fs')
const path = require('node:path')
const target = process.argv[2]
const parent = path.dirname(target)
if (!path.isAbsolute(target) || fs.realpathSync(parent) !== parent) throw new Error('ambiguous default profile parent')
fs.readdirSync(parent)
try {
  fs.lstatSync(target)
  throw new Error('default SeuOffice profile already exists')
} catch (error) {
  if (error.code !== 'ENOENT') throw error
}
NODE
then
  result "BLOCKED: cannot prove the default profile absent"
  exit 1
fi
result "ephemeral runner default profile: $ud; initially absent"
if [ -e "$app" ] || [ -L "$app" ]; then
  result "BLOCKED: $app already exists"
  exit 1
fi
probe() { node "$here/probe.mjs" "$@" --port "$port"; }
probe_failure() {
  if [ -n "$to_receipt" ] && pgrep -f "$app/Contents/MacOS/SeuOffice" >/dev/null && ! curl -fsS --max-time 5 "http://127.0.0.1:$port/json/list" >/dev/null 2>&1; then
    result "BLOCKED: candidate CDP endpoint unavailable; isolated real-page control is required for these exact candidate bytes"
  else
    result "FAILED: $1"
  fi
  exit 1
}
# --use-mock-keychain: the updated bundle carries another code signature than the
# ad hoc one the old app sealed its AI key under, and macOS then asks to allow
# the keychain item; nobody is there to click on a runner. Run 43435143003: the
# updated app listened on the debugging port for five minutes and never logged
# "running", the state of a main process waiting on that prompt. A real update
# keeps the signature, so the prompt belongs to the harness, not to the update.
# Every launch uses the same mock, so the key sealed before the update is still
# opened by the updated app.
start_app() { "$app/Contents/MacOS/SeuOffice" --remote-debugging-port=$port --use-mock-keychain "$@" >>"$out/app.log" 2>&1 & app_pid=$!; }
make_feed() {
  local dir=$1 file=$2 version=$3 yml=${4:-}
  mkdir -p "$dir"
  node "$here/feed.mjs" make --file "$file" --version "$version" --out "$dir" ${yml:+--yml "$yml"} >/dev/null
}
# what ran, what listened, what the main process was doing and what was on screen
diagnose() {
  local d="$out/diag-$1"
  mkdir -p "$d"
  { ps -axo pid,ppid,etime,stat,command | grep -i '[S]euOffice'; lsof -nP -iTCP:$port -sTCP:LISTEN; } >"$d/ps.txt" 2>&1 || true
  curl -sS --max-time 5 "http://127.0.0.1:$port/json/list" >"$d/json-list.txt" 2>&1 || true
  for p in $(pgrep -f "$app/Contents/MacOS/SeuOffice"); do sample "$p" 3 -file "$d/sample-$p.txt" >/dev/null 2>&1 || true; done
  screencapture -x "$d/screen.png" 2>/dev/null || true
  ls -la "$ud" >"$d/userdata.txt" 2>&1 || true
  cp "$ud/update-log.txt" "$d/" 2>/dev/null || true
}
# "Restart & Install" must make the running app quit by itself (the swap runs at
# its will-quit). Run 43435143003 had it hanging 3.5 minutes, until stop_app
# ended it; that must not pass unseen.
quit_hang=
expect_self_quit() {
  local pid=$app_pid
  for _ in $(seq 120); do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; done
  diagnose "quit-$1"
  quit_hang="the app did not quit within 120 s of Restart & Install ($1)"
  result "FAILED: $quit_hang"
}
stop_app() {
  osascript -e 'tell application id "com.seuput.seuoffice" to quit' >/dev/null 2>&1 || true
  for _ in $(seq 20); do pgrep -f "$app/Contents/MacOS/SeuOffice" >/dev/null || return 0; sleep 1; done
  pkill -9 -f "$app/Contents/MacOS/SeuOffice" || true
}
version() { /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist"; }
trap 'stop_app; if [ -n "${feed:-}" ]; then kill "$feed" 2>/dev/null || true; fi' EXIT

gh release download "$from" --repo seuput-inc/seuoffice --dir "$out/from" --pattern 'SeuOffice-*-arm64-mac.zip' --clobber
zip=$(ls "$out"/from/SeuOffice-*-arm64-mac.zip | grep -v stable | head -1)
ditto -x -k "$zip" /Applications
from_version=$(version)
result "installed $from_version"

make_feed "$out/feed" "$to_zip" "$to_version" "${TO_YML:-}"
node "$here/feed.mjs" serve --dir "$out/feed" --port 8765 >"$out/feed.log" 2>&1 &
feed=$!
node "$here/feed.mjs" point --resources "$app/Contents/Resources" --url http://127.0.0.1:8765/ >/dev/null
codesign --force --deep -s - "$app"

node "$here/probe.mjs" make-docx "$out/migration-test.docx"
start_app "$out/migration-test.docx"
sleep 10
probe seed --out "$out/seed.json"
stop_app
result seeded

start_app
probe update --version "$to_version"
expect_self_quit first
for _ in $(seq 180); do [ "$(version)" = "$to_version" ] && break; sleep 1; done
# the swap relaunches the app after the new bundle is in place: wait for it,
# or a later start hands over to it (single instance) and quits
for _ in $(seq 60); do pgrep -f "$app/Contents/MacOS/SeuOffice" >/dev/null && break; sleep 1; done
sleep 10
stop_app
[ "$(version)" = "$to_version" ] || { result "still $(version) after the update"; exit 1; }
result "updated to $to_version"

start_app
sleep 10
if ! probe check --seed "$out/seed.json" --version "$to_version"; then
  diagnose first-check
  probe_failure "first candidate profile/UI check"
fi
stop_app
result "checked $to_version"

if [ -n "$next_zip" ]; then
make_feed "$out/feed-next" "$next_zip" "$next_version" "${NEXT_YML:-}"
kill "$feed"
wait "$feed" 2>/dev/null || true
node "$here/feed.mjs" serve --dir "$out/feed-next" --port 8765 >"$out/feed-next.log" 2>&1 &
feed=$!
node "$here/feed.mjs" point --resources "$app/Contents/Resources" --url http://127.0.0.1:8765/ >/dev/null
codesign --force --deep -s - "$app"
start_app
if ! probe update --version "$next_version"; then
  diagnose second-update
  probe_failure "second update"
fi
expect_self_quit second
for _ in $(seq 180); do [ "$(version)" = "$next_version" ] && break; sleep 1; done
for _ in $(seq 60); do pgrep -f "$app/Contents/MacOS/SeuOffice" >/dev/null && break; sleep 1; done
sleep 10
stop_app
[ "$(version)" = "$next_version" ] || { result "still $(version) after the second update"; exit 1; }
start_app
sleep 10
if ! probe check --seed "$out/seed.json" --version "$next_version"; then
  diagnose second-check
  probe_failure "second candidate profile/UI check"
fi
stop_app
result "second update checked $next_version"
fi
[ -z "$quit_hang" ] || exit 1
result passed
node - "$out/result.json" "$from" "$to_version" "$next_version" "$to_receipt" "$next_receipt" <<'NODE'
const fs = require('node:fs')
const [output, from, to, next, toPath, nextPath] = process.argv.slice(2)
const receipt = (file) => file ? JSON.parse(fs.readFileSync(file, 'utf8')) : null
fs.writeFileSync(output, JSON.stringify({
  from, to, next, passed: true,
  toReceipt: receipt(toPath), nextReceipt: receipt(nextPath),
  checks: ['profile-initially-absent', 'seeded', 'candidate-ui-data', ...(next ? ['second-update-ui-data'] : [])],
}, null, 2) + '\n')
NODE
