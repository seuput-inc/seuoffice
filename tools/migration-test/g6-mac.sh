#!/bin/bash
set -Eeuo pipefail
umask 077
phase=preflight
blocked() { printf 'G6_BLOCKED %s\n' "$1"; printf 'G6 BLOCKED (macOS, %s): %s\n' "$phase" "$1" >&2; exit 3; }
failed() { printf 'G6 FAILED (macOS, %s): %s\n' "$phase" "$1" >&2; exit 1; }
trap 'failed "command failed at line $LINENO"' ERR

[[ ${GITHUB_ACTIONS:-} == true && ${RUNNER_ENVIRONMENT:-} == github-hosted &&
   ${RUNNER_OS:-} == macOS && ${GITHUB_EVENT_NAME:-} == workflow_dispatch &&
   ${GITHUB_REPOSITORY:-} == seuput-inc/seuoffice &&
   ${GITHUB_REF:-} == refs/heads/integration/tauri-first-candidate &&
   $(/usr/bin/uname -sm) == 'Darwin arm64' && $(/usr/bin/id -un) == runner &&
   $HOME == /Users/runner && ${RUNNER_TEMP:-} == /Users/runner/work/_temp ]] ||
  blocked 'requires the disposable hosted Mac candidate dispatch and its default runner account'
[[ $(/usr/bin/stat -f %Su /dev/console) == runner && -z ${SEUOFFICE_USER_DATA:-} &&
   -z ${SEUOFFICE_FAKE_UPDATE:-} && -z ${ELECTRON_RUN_AS_NODE:-} ]] ||
  blocked 'requires the default interactive runner session without profile or updater overrides'

root=$RUNNER_TEMP/g6-qa
profile="$HOME/Library/Application Support/seuoffice"
app=/Applications/SeuOffice.app
exe=$app/Contents/MacOS/SeuOffice
here=$(cd "$(dirname "$0")" && pwd -P)
[[ ${G6_PLAN:-} == "$root/plan.json" && ${G6_OUT:-} == "$root/driver" &&
   ${G6_FEED_CONTROL:-} == "$root/stage" && -d $G6_OUT && -f $G6_PLAN &&
   -f $G6_FEED_CONTROL && $(cat "$G6_FEED_CONTROL") == first ]] ||
  blocked 'unexpected QA plan, output or initial feed stage'
for entry in "$G6_PLAN" "$G6_OUT" "$G6_FEED_CONTROL" "${profile%/*}" /Applications; do
  [[ $(realpath "$entry") == "$entry" && -r $entry ]] || blocked 'ambiguous runner paths'
done
[[ -x ${profile%/*} && -w /Applications ]] || blocked 'default application/profile locations inaccessible'
for entry in "$profile" "$app" "$HOME/Applications/SeuOffice.app" \
    /Applications/.SeuOffice.app.update /Applications/.SeuOffice.app.previous \
    "$HOME/Library/Caches/seuoffice-updater"; do
  [[ ! -e $entry && ! -L $entry ]] || blocked 'existing installation, updater cache, staging area or profile'
done
if pgrep -x SeuOffice >/dev/null; then blocked 'SeuOffice is already running'; fi

work=$(mktemp -d "$G6_OUT/work.XXXXXXXX")
trap 'rm -rf "$work"' EXIT
channel=$(plutil -extract channel raw -o - "$G6_PLAN")
[[ $channel == latest || $channel == stable ]] || failed 'unknown feed channel'
suffix=
[[ $channel != stable ]] || suffix=-stable
version() { plutil -extract "$1.version" raw -o - "$G6_PLAN"; }
hashfile() { shasum -a 256 "$1" | cut -d' ' -f1; }
identity() {
  local bundle=$1 version=$2 info="$1/Contents/Info.plist" code="$1/Contents/MacOS/SeuOffice" cdhash
  [[ -d $bundle && ! -L $bundle && -f $code && ! -L $code && -x $code &&
     $(plutil -extract CFBundleIdentifier raw -o - "$info") == com.seuput.seuoffice &&
     $(plutil -extract CFBundleExecutable raw -o - "$info") == SeuOffice &&
     $(plutil -extract CFBundleShortVersionString raw -o - "$info") == "$version" ]] || return 1
  /bin/bash "$here/../../crates/shell/ci/mac-check-signature.sh" "$bundle" >&2 || return 1
  cdhash=$(codesign -dv --verbose=4 "$bundle" 2>&1 | sed -n 's/^CDHash=//p') || return 1
  [[ $cdhash =~ ^[0-9a-fA-F]{40}$ ]] || return 1
  printf '%s ' "$cdhash"; hashfile "$code"
}

for stage in bridge first next; do
  phase="verify $stage ZIP"
  v=$(version "$stage")
  [[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || failed 'invalid package version'
  zip=$(plutil -extract "$stage.installer" raw -o - "$G6_PLAN")
  name="SeuOffice-$v-arm64-mac-qa-loopback$suffix.zip"
  if [[ $stage == bridge ]]; then name="SeuOffice-$v-arm64-mac$suffix-migration-seam.zip"; fi
  [[ $zip == "$root/"* && $(basename "$zip") == "$name" && -f $zip &&
     $(realpath "$zip") == "$zip" ]] || failed 'expected the selected immutable QA update ZIP'
  mkdir "$work/unpacked"
  ditto -x -k "$zip" "$work/unpacked"
  source=$work/unpacked/SeuOffice.app
  identity "$source" "$v" > "$work/$stage.identity" || failed 'invalid signed bundle in QA ZIP'
  config=$source/Contents/Resources/app-update.yml
  [[ -f $config && ! -L $config ]] || failed 'missing packaged updater configuration'
  /usr/bin/ruby -ryaml -e '
    expected = {"provider"=>"generic", "url"=>"http://127.0.0.1:8765/", "channel"=>ARGV[1], "updaterCacheDirName"=>"seuoffice-updater"}
    text = File.read(ARGV[0])
    abort unless YAML.safe_load(text) == expected && YAML.parse_stream(text).children.size == 1 &&
      YAML.parse(text).root.children.size == 2 * expected.size
  ' "$config" "$channel" || failed 'expected only the fixed loopback provider and selected channel'
  if [[ $stage == bridge ]]; then mv "$source" "$work/bridge.app"; fi
  rm -rf "$work/unpacked"
done

phase='install QA bridge'
/usr/bin/xcrun swiftc "$here/g6-mac-ui.swift" -o "$work/ui" || failed 'could not build native UI reader'
ditto "$work/bridge.app" "$app"
rm -rf "$work/bridge.app"
verify_install() {
  local actual
  actual=$(identity "$app" "$(version "$1")") || failed 'installed signature, bundle ID or exact version mismatch'
  [[ $actual == "$(cat "$work/$1.identity")" ]] || failed 'installed code directory or executable SHA-256 differs from selected ZIP'
}
verify_install bridge
doc="$work/g6-$(uuidgen).docx"
node "$here/probe.mjs" make-docx "$doc"
digest=$(hashfile "$doc")
open -a "$app" "$doc"

pid_of_app() {
  local pid
  pid=$(pgrep -u runner -x SeuOffice) || return 1
  [[ $pid =~ ^[0-9]+$ && $(ps -p "$pid" -o comm=) == "$exe" ]] || return 1
  lsof -nP -a -p "$pid" -d txt "$exe" >/dev/null 2>&1 || return 1
  printf '%s' "$pid"
}
wait_pid() {
  local previous=$1 pid end=$((SECONDS + 180))
  while (( SECONDS < end )); do
    if pid=$(pid_of_app) && [[ $pid != "$previous" ]] &&
      { [[ -z $previous ]] || ! kill -0 "$previous" 2>/dev/null; }; then
      printf '%s' "$pid"; return 0
    fi
    sleep 1
  done
  return 1
}
ui() {
  local status=0
  "$work/ui" "$@" || status=$?
  case "$status" in
    0) ;;
    3) exit 3 ;;
    *) failed 'installed UI did not show or complete the expected action' ;;
  esac
}
phase='bridge profile'
pid=$(wait_pid '') || failed 'QA bridge did not start from the installed executable'
ui seed "$pid" "$(basename "$doc")" "$(version bridge)"
[[ -d $profile && $(realpath "$profile") == "$profile" && $(stat -f %Su "$profile") == runner ]] ||
  failed 'bridge did not create the default profile'
lsof -nP -a -p "$pid" -Fn | awk -v p="n$profile/" 'index($0,p)==1 {seen=1} END {exit !seen}' ||
  failed 'bridge process has no open file under the selected profile'
profile_id=$(stat -f '%d:%i' "$profile")

current=bridge
for stage in first next; do
  phase="$current to $stage"
  v=$(version "$stage")
  if [[ $stage == next ]]; then
    printf next > "$work/next-stage"
    mv -f "$work/next-stage" "$G6_FEED_CONTROL"
  fi
  ui update "$pid" "$(version "$current")" "$v"
  pid=$(wait_pid "$pid") || failed 'updater did not replace the old process with a new installed app'
  verify_install "$stage"
  ui arrival "$pid" "$(basename "$doc")" "$v"
  [[ $(realpath "$profile") == "$profile" && $(stat -f '%d:%i' "$profile") == "$profile_id" ]] ||
    failed 'profile path or directory identity changed'
  lsof -nP -a -p "$pid" "$profile/shell-instance.lock" >/dev/null ||
    failed 'new Tauri process does not hold the original profile instance file'
  [[ $(pid_of_app) == "$pid" && -f $doc && ! -L $doc && $(hashfile "$doc") == "$digest" ]] ||
    failed 'new process or synthetic document changed during verification'
  printf 'G6_EVENT {"stage":"%s","version":"%s","profile":"%s","document":"%s","recent":true}\n' \
    "$stage" "$v" "$profile" "$digest"
  current=$stage
done
