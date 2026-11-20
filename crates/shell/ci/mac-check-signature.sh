#!/usr/bin/env bash
# Fails unless every app bundle given is intact and signed by the fixed
# self-signed identity of decision 0035: not ad hoc, and a designated
# requirement that names the certificate, so the keychain keeps trusting the
# next version.
set -euo pipefail

for app in "$@"; do
  codesign --verify --deep --strict --verbose=2 "$app"
  details=$(codesign -dv --verbose=2 "$app" 2>&1)
  if grep -q 'Signature=adhoc' <<<"$details" || ! grep -Fxq 'Authority=SeuOffice Self Sign' <<<"$details"; then
    echo "::error::$app is not signed by SeuOffice Self Sign"
    echo "$details"
    exit 1
  fi
  requirement=$(codesign -d -r- "$app" 2>&1)
  if ! grep -q 'identifier "com.seuput.seuoffice"' <<<"$requirement" || ! grep -Eq 'certificate (leaf|root) = H"' <<<"$requirement"; then
    echo "::error::$app has no certificate-based designated requirement"
    echo "$requirement"
    exit 1
  fi
done
