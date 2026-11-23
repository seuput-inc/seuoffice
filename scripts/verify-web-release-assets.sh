#!/usr/bin/env bash
# Read the GitHub Releases API response for this tag on stdin.
set -euo pipefail
tag=${1:?expected a release tag}
dir=${2:?expected the directory containing the downloaded tarballs}
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "invalid release tag: $tag" >&2; exit 1; }
release=$(cat)
for name in embed collab web history-renderer; do
  file="seuoffice-$name-${tag#v}.tgz"
  digest=$(jq -er --arg tag "$tag" --arg file "$file" '
    select(.tag_name == $tag) | [.assets[] | select(.name == $file and .state == "uploaded")]
    | select(length == 1) | .[0].digest | strings | select(test("^sha256:[0-9a-f]{64}$"))
  ' <<< "$release") || { echo "missing GitHub release digest for $tag/$file" >&2; exit 1; }
  actual=$(shasum -b -a 256 < "$dir/$file")
  [[ "sha256:${actual%% *}" == "$digest" ]] || { echo "digest mismatch: $file" >&2; exit 1; }
  printf '%s  %s\n' "$digest" "$file"
done
