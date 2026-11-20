#!/usr/bin/env bash
# Puts the fixed self-signed code-signing identity (decision 0035) into a
# keychain of this run and writes its SHA-1 to $GITHUB_ENV as
# SEUOFFICE_MAC_SIGN_IDENTITY, for `APPLE_SIGNING_IDENTITY` of `tauri build`.
#
# Input: MAC_SELF_SIGN_P12 (base64 of the p12) and MAC_SELF_SIGN_PASSWORD,
# from the repository secrets. Neither value is ever printed; the p12 lives
# on disk only between decode and import. The certificate's SHA-1 is public.
set -euo pipefail

case "${GITHUB_EVENT_NAME:-}" in
  pull_request | pull_request_target)
    echo '::error::the signing identity is not used for pull requests'
    exit 1
    ;;
esac
if [ -z "${MAC_SELF_SIGN_P12:-}" ] || [ -z "${MAC_SELF_SIGN_PASSWORD:-}" ]; then
  echo '::error::MAC_SELF_SIGN_P12 and MAC_SELF_SIGN_PASSWORD are required (decision 0035)'
  exit 1
fi

keychain="$RUNNER_TEMP/seuoffice-sign.keychain-db"
keychain_password=$(openssl rand -hex 24)
echo "::add-mask::$keychain_password"
p12="$RUNNER_TEMP/seuoffice-sign.p12"
trap 'rm -P -f "$p12" 2>/dev/null || rm -f "$p12"' EXIT

security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
(umask 077 && printf %s "$MAC_SELF_SIGN_P12" | base64 --decode > "$p12")
security import "$p12" -k "$keychain" -f pkcs12 -P "$MAC_SELF_SIGN_PASSWORD" -T /usr/bin/codesign > /dev/null
# codesign reads the key without a dialog only when its partition is listed
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" > /dev/null
# codesign finds an identity by name or hash in the search list only
existing=$(security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"$//')
# shellcheck disable=SC2086
security list-keychains -d user -s "$keychain" $existing

identity=$(security find-identity -p codesigning "$keychain" | awk '/"SeuOffice Self Sign"/ { print $2; exit }')
if ! [[ "$identity" =~ ^[0-9A-F]{40}$ ]]; then
  echo '::error::the SeuOffice Self Sign identity is not in the imported p12'
  exit 1
fi
echo "SEUOFFICE_MAC_SIGN_IDENTITY=$identity" >> "$GITHUB_ENV"
echo "signing identity SeuOffice Self Sign, SHA-1 $identity"
