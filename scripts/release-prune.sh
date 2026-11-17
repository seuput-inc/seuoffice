#!/usr/bin/env bash
# Deletes published releases of $GH_REPO except: the release GitHub marks Latest (the stable update channel and the
# download button read only it), the newest `stable` non-prerelease releases, the newest `prerelease` pre-releases, and
# the tags .github/release-keep.json names. Only the release and its assets go; git tags are never deleted
# (no --cleanup-tag). "Newest" means highest semantic version of vX.Y.Z tags, not creation time. Drafts and releases
# whose tag is not vX.Y.Z are left alone.
# `--dry-run` prints the plan and deletes nothing. Stops with nothing deleted when Latest cannot be read or is not among
# the releases, when a keep tag is not a tag of the repo, when the survivors miss Latest or hold fewer stable releases
# than min(`stable`, stable releases that exist), or when more than MAX_DELETE would go in one run.
set -euo pipefail

dry=${1:-}
: "${GH_REPO:?GH_REPO must be set (owner/name)}"
keep_file=${KEEP_FILE:-.github/release-keep.json}
max_delete=${MAX_DELETE:-20}
n_stable=$(jq -er '.stable' "$keep_file")
n_pre=$(jq -er '.prerelease' "$keep_file")
[ "$n_stable" -ge 1 ] && [ "$n_pre" -ge 0 ] || { echo "::error::$keep_file: stable must be at least 1, prerelease at least 0"; exit 1; }

latest_tag=$(gh api "repos/$GH_REPO/releases/latest" --jq .tag_name) && [ -n "$latest_tag" ] \
  || { echo "::error::cannot read the Latest release of $GH_REPO: nothing deleted"; exit 1; }

keep_tags=()
while IFS= read -r t; do keep_tags+=("$t"); done < <(jq -er '.keep | keys[]' "$keep_file")
for t in ${keep_tags[@]+"${keep_tags[@]}"}; do
  gh api "repos/$GH_REPO/git/ref/tags/$t" --silent 2>/dev/null \
    || { echo "::error::keep-list tag $t is not a tag of $GH_REPO"; exit 1; }
done

rows=$(gh api --paginate "repos/$GH_REPO/releases?per_page=100" \
  --jq '.[] | select(.draft == false) | [.tag_name, .prerelease] | @tsv')
semver_desc() { grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -t. -k1.2,1nr -k2,2nr -k3,3nr || true; }
stable_list=$(awk -F'\t' '$2 == "false" {print $1}' <<<"$rows" | semver_desc)
pre_list=$(awk -F'\t' '$2 == "true" {print $1}' <<<"$rows" | semver_desc)
all_list=$(printf '%s\n%s\n' "$stable_list" "$pre_list" | grep . | sort -u || true)
grep -qxF "$latest_tag" <<<"$all_list" || { echo "::error::Latest $latest_tag is not among the published releases: nothing deleted"; exit 1; }

reasons=
add() { reasons+="$1"$'\t'"$2"$'\n'; }
why() { awk -F'\t' -v t="$1" '$1 == t {printf "%s%s", sep, $2; sep = "; "}' <<<"$reasons"; }
add "$latest_tag" "Latest"
i=0; while IFS= read -r t; do [ -n "$t" ] || continue; i=$((i + 1)); [ "$i" -gt "$n_stable" ] || add "$t" "newest stable $i of $n_stable"; done <<<"$stable_list"
i=0; while IFS= read -r t; do [ -n "$t" ] || continue; i=$((i + 1)); [ "$i" -gt "$n_pre" ] || add "$t" "newest pre-release $i of $n_pre"; done <<<"$pre_list"
for k in ${keep_tags[@]+"${keep_tags[@]}"}; do add "$k" "$(jq -r --arg t "$k" '.keep[$t]' "$keep_file")"; done

echo "Latest = $latest_tag"
doomed=(); survivors=(); stable_surv=0
while IFS= read -r tag; do
  [ -n "$tag" ] || continue
  reason=$(why "$tag")
  if [ -n "$reason" ]; then
    survivors+=("$tag"); echo "keep $tag: $reason"
    ! grep -qxF "$tag" <<<"$stable_list" || stable_surv=$((stable_surv + 1))
  else doomed+=("$tag"); fi
done < <(sort -t. -k1.2,1nr -k2,2nr -k3,3nr <<<"$all_list")

n_stable_exist=$(grep -c . <<<"$stable_list" || true)
want=$(( n_stable < n_stable_exist ? n_stable : n_stable_exist ))
if [ -z "$(why "$latest_tag")" ] || [ "$stable_surv" -lt "$want" ]; then
  echo "::error::survivors hold $stable_surv stable releases, fewer than $want (or miss Latest): nothing deleted"; exit 1
fi
if [ "${#doomed[@]}" -gt "$max_delete" ]; then
  echo "::error::${#doomed[@]} deletions exceed the bound of $max_delete: nothing deleted"; exit 1
fi

echo "delete ${#doomed[@]}"
for tag in ${doomed[@]+"${doomed[@]}"}; do
  if [ "$dry" = --dry-run ]; then echo "would delete $tag"; else gh release delete "$tag" --yes; echo "deleted $tag (tag kept)"; fi
done
