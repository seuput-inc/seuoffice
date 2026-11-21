#!/usr/bin/env bash
# Fails unless every app bundle given is intact and signed by the fixed
# self-signed identity of decision 0035: not ad hoc, and a designated
# requirement that names the certificate, so the keychain keeps trusting the
# next version.
set -euo pipefail

identity=${SEUOFFICE_MAC_SIGN_IDENTITY:-}
if [[ $# -eq 0 || ! "$identity" =~ ^[0-9A-F]{40}$ ]]; then
  echo '::error::expected a bundle and the imported decision 0035 certificate fingerprint'
  exit 1
fi
id='identifier "com.seuput.seuoffice"'
leaf="certificate leaf = H\"$identity\""

for app in "$@"; do
  codesign --verify --deep --strict --verbose=2 "$app"
  codesign --verify "-R=$id and $leaf" "$app"
  details=$(codesign -dv --verbose=2 "$app" 2>&1)
  if grep -q 'Signature=adhoc' <<<"$details" || ! grep -Fxq 'Authority=SeuOffice Self Sign' <<<"$details"; then
    echo "::error::$app is not signed by SeuOffice Self Sign"
    echo "$details"
    exit 1
  fi
  requirement=$(codesign -d -r- "$app" 2>&1)
  designated=$(sed -n 's/^designated => //p' <<<"$requirement")
  # A matching name or a weaker requirement with an alternative signer is not the same identity.
  shopt -s nocasematch
  if [[ "$designated" != "$id and $leaf" && "$designated" != "$id and certificate root = H\"$identity\"" ]]; then
    echo "::error::$app does not designate the fixed decision 0035 identity"
    echo "$requirement"
    exit 1
  fi
done
