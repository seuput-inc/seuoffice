#!/usr/bin/env bash
# The migration acceptance test of decision 0029 on a macOS runner: the path
# of win-desktop.ps1 without the faults. Install the Electron release FROM in
# /Applications, open a mock document, seed data, point the updater at a
# local feed offering TO_ZIP, update through the app's dialog (mac-update.ts
# stages and swaps the bundle), and check the updated app shows the data.
#
#   mac-runner.sh <from tag> <to zip> <to version> <out dir> <next zip> <next version> [to receipt] [next receipt]
#
# Two updates are made from the same release, each in a profile of its own:
#   clean    the mock document is open and unchanged at "Restart & Install"
#   unsaved  text is typed into the open document first: the app must ask whether
#            to save, stay running at the question, and keep the text. "Cancel"
#            answers it once (the app and the text stay); "Save" the second time,
#            and the update goes on. The note on a quit that needed a second
#            request is below. The second update (NEXT_ZIP) follows this one.
#
# Every quit is judged by quit-policy.mjs by the version that quits: up to 0.12.43
# a quit with a Word tab open left the process running (the shell window's close
# prompt called the quit off, and macOS does not quit on its last window closing),
# so one second quit request is allowed and recorded as a note; from 0.12.44 the
# first request must end the app.
#
# The bundle is signed again ad hoc after app-update.yml is rewritten; the
# AI key is seeded after that, so the keychain item belongs to the signed
# app (decision 0029 (b): a new signature asks once, as every update does).
#
# The question about unsaved changes is a native sheet: it is read and answered through
# System Events (the runner must allow it to control the app). Without that the run
# fails and says what the system answered.
set -euo pipefail
from=$1 to_zip=$2 to_version=$3 out=$4 next_zip=$5 next_version=$6
to_receipt=${7:-} next_receipt=${8:-}
[ "${GITHUB_ACTIONS:-}" = true ] && [ "${RUNNER_ENVIRONMENT:-}" = github-hosted ] && [ -n "${RUNNER_TEMP:-}" ] || { echo "Migration requires a GitHub-hosted runner and RUNNER_TEMP" >&2; exit 1; }
default_ud="$HOME/Library/Application Support/seuoffice"
ud="$default_ud"
export SEUOFFICE_USER_DATA="$ud"
here=$(cd "$(dirname "$0")" && pwd)
app=/Applications/SeuOffice.app
bundle_id=com.seuput.seuoffice
port=9229
# how long the first quit request has to end the app
first_window=${QUIT_FIRST_WINDOW:-30}
mkdir -p "$out/from"
: >"$out/notes.tsv"
[ -z "$to_receipt" ] || cp "$to_receipt" "$out/to-receipt.json"
[ -z "$next_receipt" ] || cp "$next_receipt" "$out/next-receipt.json"
result() { printf '%s\n' "$1" >>"$out/result.txt"; echo "== $1"; }
if ! node - "$default_ud" <<'NODE'
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
result "ephemeral runner default profile: $default_ud; initially absent"
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
  ui_dump >"$d/ui.txt" 2>&1 || true
  ls -la "$ud" >"$d/userdata.txt" 2>&1 || true
  cp "$ud/update-log.txt" "$d/" 2>/dev/null || true
}

# ---- quitting ----
quit_target=
ask_quit() { osascript -e "tell application id \"$bundle_id\" to quit" >/dev/null 2>&1 || true; }
quit_done() { if [ -n "$quit_target" ]; then ! kill -0 "$quit_target" 2>/dev/null; else ! pgrep -f "$app/Contents/MacOS/SeuOffice" >/dev/null; fi; }
wait_until() {
  local seconds=$1; shift
  for _ in $(seq "$seconds"); do "$@" && return 0; sleep 1; done
  return 1
}
quit_failed=
# The quit of the running app, judged by quit-policy.mjs: $1 names it, $2 is the version
# that quits, $3 is "ask" (the request is sent here) or "asked" (it was sent already, by the
# app itself: Restart & Install), $4 the process to wait for (default: the app's processes,
# which is wrong after an update, when the swap starts the new app at once).
settle_quit() {
  local label=$1 quitting=$2 how=$3 first=no second_sent=no second=no rc=0 verdict note reason
  quit_target=${4:-}
  if [ "$how" = ask ]; then ask_quit; fi
  if wait_until "$first_window" quit_done; then first=yes; fi
  verdict=$(node "$here/quit-policy.mjs" "$quitting" "$first") || rc=$?
  if [ "$rc" = 10 ]; then
    ask_quit
    second_sent=yes
    if wait_until "$first_window" quit_done; then second=yes; fi
    rc=0
    verdict=$(node "$here/quit-policy.mjs" "$quitting" no yes "$second") || rc=$?
  fi
  if [ "$rc" = 0 ]; then
    note=$(node -p 'JSON.parse(process.argv[1]).note ?? ""' "$verdict")
    if [ -n "$note" ]; then
      printf '%s\t%s\t%s\n' "$label" "$quitting" "$note" >>"$out/notes.tsv"
      result "$note"
      result "  (the quit of $quitting, $label)"
    fi
    return 0
  fi
  reason=$(node -p 'JSON.parse(process.argv[1]).reason ?? "the quit policy said no"' "$verdict" 2>/dev/null || echo "the quit policy could not be read")
  diagnose "quit-$label"
  quit_failed="the app did not quit as asked ($label): $reason"
  result "FAILED: $quit_failed"
  pkill -9 -f "$app/Contents/MacOS/SeuOffice" || true
  quit_target=
}
stop_app() {
  if ! pgrep -f "$app/Contents/MacOS/SeuOffice" >/dev/null; then return 0; fi
  settle_quit "${1:-stop}" "$(version)" ask
}
# the last resort of the exit trap: nothing may outlive the run
force_stop() {
  ask_quit
  wait_until 10 quit_done || true
  pkill -9 -f "$app/Contents/MacOS/SeuOffice" || true
}
version() { /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist"; }
trap 'force_stop; if [ -n "${feed:-}" ]; then kill "$feed" 2>/dev/null || true; fi' EXIT

# ---- the question about unsaved changes (a native sheet), through System Events ----
# the buttons of the sheet, "|" between: or no-sheet / no-process; fails with the system's answer
ui_buttons() {
  osascript <<'APPLESCRIPT' 2>&1
with timeout of 30 seconds
  tell application "System Events"
    if not (exists process "SeuOffice") then return "no-process"
    tell process "SeuOffice"
      repeat with w in windows
        if (count of sheets of w) > 0 then
          set names to {}
          repeat with b in (buttons of sheet 1 of w)
            set end of names to name of b
          end repeat
          set AppleScript's text item delimiters to "|"
          return names as text
        end if
      end repeat
    end tell
  end tell
  return "no-sheet"
end timeout
APPLESCRIPT
}
ui_click() {
  osascript - "$1" <<'APPLESCRIPT' 2>&1
on run argv
  set wanted to item 1 of argv
  with timeout of 30 seconds
    tell application "System Events"
      tell process "SeuOffice"
        repeat with w in windows
          if (count of sheets of w) > 0 then
            click button wanted of sheet 1 of w
            return "clicked " & wanted
          end if
        end repeat
      end tell
    end tell
    return "no-sheet"
  end timeout
end run
APPLESCRIPT
}
ui_dump() {
  osascript -e 'with timeout of 30 seconds' -e 'tell application "System Events" to tell process "SeuOffice" to return entire contents of window 1' -e 'end timeout' 2>&1
}
prompt_up() { ui_buttons >"$out/ui-buttons.txt" 2>&1 && grep -q 'Save' "$out/ui-buttons.txt" && grep -q 'Cancel' "$out/ui-buttons.txt"; }
prompt_gone() { [ "$(ui_buttons 2>&1)" = no-sheet ]; }
doc_has() { unzip -p "$1" word/document.xml 2>/dev/null | grep -qF -- "$2"; }
app_alive() { kill -0 "$app_pid" 2>/dev/null; }
no_install_began() { ! grep -q 'phase will-quit' "$ud/update-log.txt" 2>/dev/null; }

unsaved_fail() {
  diagnose "unsaved-$1"
  result "FAILED: unsaved document: $2"
  exit 1
}
# what the app does with a document that has unsaved changes when Restart & Install quits it
# $1 the document, $2 the text typed into it
unsaved_prompt() {
  local doc=$1 marker=$2 before
  before=$(shasum -a 256 "$doc" | cut -d' ' -f1)
  probe dirty --marker "$marker" || unsaved_fail dirty "the text could not be typed into the open document"
  result "[unsaved] typed $marker into the open document"
  for round in cancel save; do
    probe update --version "$to_version"
    if ! wait_until 60 prompt_up; then
      unsaved_fail "$round-prompt" "no question about unsaved changes within 60 s of Restart & Install (sheet: $(cat "$out/ui-buttons.txt" 2>/dev/null))"
    fi
    result "[unsaved] the app asked: $(cat "$out/ui-buttons.txt")"
    sleep 10
    app_alive || unsaved_fail "$round-quit" "the app quit with unsaved changes while the question was open"
    no_install_began || unsaved_fail "$round-install" "the update began to install while the question was open"
    [ "$(shasum -a 256 "$doc" | cut -d' ' -f1)" = "$before" ] || unsaved_fail "$round-file" "the document file changed before the question was answered"
    probe editor --marker "$marker" >/dev/null || unsaved_fail "$round-text" "the typed text is gone from the editor while the question is open"
    screencapture -x "$out/unsaved-$round-prompt.png" 2>/dev/null || true
    if [ "$round" = cancel ]; then
      ui_click Cancel >"$out/ui-click.txt" 2>&1 || unsaved_fail cancel-click "the sheet could not be answered: $(cat "$out/ui-click.txt")"
      wait_until 20 prompt_gone || unsaved_fail cancel-gone "the question stayed after Cancel"
      sleep 5
      app_alive || unsaved_fail cancel-quit "the app quit after Cancel"
      probe editor --marker "$marker" >/dev/null || unsaved_fail cancel-text "the typed text is gone after Cancel"
      no_install_began || unsaved_fail cancel-install "the update began to install after Cancel"
      result "[unsaved] Cancel kept the app, the question closed, the text stayed in the editor"
    fi
  done
  ui_click Save >"$out/ui-click.txt" 2>&1 || unsaved_fail save-click "the sheet could not be answered: $(cat "$out/ui-click.txt")"
  wait_until 60 doc_has "$doc" "$marker" || unsaved_fail save-file "the document file does not hold the typed text 60 s after Save"
  result "[unsaved] Save wrote the typed text into the document file"
}

# ---- one update from the installed release to TO ----
install_from() {
  rm -rf "$app" "$(dirname "$app")/.SeuOffice.app.update" "$(dirname "$app")/.SeuOffice.app.previous"
  ditto -x -k "$from_zip" /Applications
  node "$here/feed.mjs" point --resources "$app/Contents/Resources" --url http://127.0.0.1:8765/ >/dev/null
  codesign --force --deep -s - "$app"
}
wait_docs_page() { wait_until 60 bash -c "curl -fsS --max-time 5 http://127.0.0.1:$port/json/list 2>/dev/null | grep -q 'docs/index.html'"; }
# $1 clean or unsaved, $2 the profile folder of the run
update_path() {
  local mode=$1 doc="$out/migration-test-$1.docx" seed="$out/seed-$1.json" marker from_version
  ud=$2
  export SEUOFFICE_USER_DATA="$ud"
  install_from
  from_version=$(version)
  result "[$mode] installed $from_version"
  node "$here/probe.mjs" make-docx "$doc"
  start_app "$doc"
  sleep 10
  probe seed --out "$seed"
  stop_app "$mode-seeded"
  result "[$mode] seeded"

  # the document open again, the way a person has it when the update comes
  start_app "$doc"
  wait_docs_page || { diagnose "$mode-no-docs-page"; result "FAILED: [$mode] the document did not open"; exit 1; }
  sleep 5
  if [ "$mode" = unsaved ]; then
    marker="MIGRATIONDIRTY$(date +%s)"
    unsaved_prompt "$doc" "$marker"
  else
    probe update --version "$to_version"
  fi
  settle_quit "$mode-restart-install" "$from_version" asked "$app_pid"
  for _ in $(seq 180); do [ "$(version)" = "$to_version" ] && break; sleep 1; done
  # the swap relaunches the app after the new bundle is in place: wait for it,
  # or a later start hands over to it (single instance) and quits
  for _ in $(seq 60); do pgrep -f "$app/Contents/MacOS/SeuOffice" >/dev/null && break; sleep 1; done
  sleep 10
  stop_app "$mode-updated"
  [ "$(version)" = "$to_version" ] || { result "[$mode] still $(version) after the update"; exit 1; }
  result "[$mode] updated to $to_version"

  start_app
  sleep 10
  if ! probe check --seed "$seed" --version "$to_version"; then
    diagnose "$mode-first-check"
    probe_failure "[$mode] first candidate profile/UI check"
  fi
  stop_app "$mode-checked"
  result "[$mode] checked $to_version"
  # the document the update found open: still in its file, with what was typed
  doc_has "$doc" '넘어가기' || { result "FAILED: [$mode] the document lost its original text"; exit 1; }
  if [ "$mode" = unsaved ]; then
    doc_has "$doc" "$marker" || { result "FAILED: [$mode] the document lost the typed text"; exit 1; }
  fi
  result "[$mode] the document that was open keeps its text"
}

gh release download "$from" --repo seuput-inc/seuoffice --dir "$out/from" --pattern 'SeuOffice-*-arm64-mac.zip' --clobber
from_zip=$(ls "$out"/from/SeuOffice-*-arm64-mac.zip | grep -v stable | head -1)

make_feed "$out/feed" "$to_zip" "$to_version" "${TO_YML:-}"
node "$here/feed.mjs" serve --dir "$out/feed" --port 8765 >"$out/feed.log" 2>&1 &
feed=$!

update_path clean "$RUNNER_TEMP/profile-clean"
update_path unsaved "$default_ud"
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
settle_quit second-restart-install "$to_version" asked "$app_pid"
for _ in $(seq 180); do [ "$(version)" = "$next_version" ] && break; sleep 1; done
for _ in $(seq 60); do pgrep -f "$app/Contents/MacOS/SeuOffice" >/dev/null && break; sleep 1; done
sleep 10
stop_app second-updated
[ "$(version)" = "$next_version" ] || { result "still $(version) after the second update"; exit 1; }
start_app
sleep 10
if ! probe check --seed "$out/seed-unsaved.json" --version "$next_version"; then
  diagnose second-check
  probe_failure "second candidate profile/UI check"
fi
stop_app second-checked
result "second update checked $next_version"
fi
[ -z "$quit_failed" ] || exit 1
result passed
node - "$out/result.json" "$from" "$to_version" "$next_version" "$to_receipt" "$next_receipt" "$out/notes.tsv" <<'NODE'
const fs = require('node:fs')
const [output, from, to, next, toPath, nextPath, notesPath] = process.argv.slice(2)
const receipt = (file) => file ? JSON.parse(fs.readFileSync(file, 'utf8')) : null
const notes = fs.readFileSync(notesPath, 'utf8').split('\n').filter(Boolean)
  .map((line) => { const [label, version, note] = line.split('\t'); return { label, version, note } })
fs.writeFileSync(output, JSON.stringify({
  from, to, next, passed: true,
  toReceipt: receipt(toPath), nextReceipt: receipt(nextPath),
  checks: [
    'profile-initially-absent', 'seeded', 'candidate-ui-data',
    ...(next ? ['second-update-ui-data'] : []),
    'clean-document-update', 'unsaved-document-prompt', 'unsaved-text-kept',
  ],
  notes,
}, null, 2) + '\n')
NODE
