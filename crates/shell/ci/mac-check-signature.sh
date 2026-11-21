#!/usr/bin/env bash
# Ad hoc signing seals the executable and resources; it does not identify a publisher.
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo '::error::expected at least one app bundle'
  exit 1
fi

for app in "$@"; do
  codesign --verify --deep --strict --verbose=2 "$app"
  codesign --verify '-R=identifier "com.seuput.seuoffice"' "$app"
  details=$(codesign -dv --verbose=2 "$app" 2>&1)
  if ! grep -Fxq 'Signature=adhoc' <<<"$details"; then
    echo "::error::$app does not have an ad hoc signature"
    echo "$details"
    exit 1
  fi
done
