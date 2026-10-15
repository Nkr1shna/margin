#!/bin/sh
# Cherry-picks chosen dev PRs onto the checked-out promote branch (docs/plan-deploy.md §4a).
# promote-release.yml cuts the branch from origin/release and calls this; it runs the
# same way by hand or in a test against any clone (deploy/scripts/promote-release-test.sh):
#
#   promote-release.sh BASE DEV PRMAP "PR PR ..." [allow-overlap]
#
# BASE   the release ref the branch was cut from (origin/release)
# DEV    the dev ref the PRs were merged into (origin/dev)
# PRMAP  a file of lines "<commit sha> <pr number> <state>": every PR merged into dev, its
#        commit on dev (the squash commit, or the merge commit of an older PR) and its state:
#        verified (labelled verified-on-dev), shipped (labelled shipped) or pending (neither)
# PRs    the PRs to promote, in any order; they are applied in dev order
#
# Refuses (exit 1, naming the PR) before changing anything when:
#   - a chosen PR isn't merged into dev, is already shipped, or isn't verified-on-dev;
#   - a chosen PR's commit touches a file that an EARLIER dev commit also touched, and that
#     earlier commit is neither chosen nor shipped (its PR isn't verified, or it was pushed
#     without a PR). The chosen change may build on it, so promoting it alone could ship
#     half a feature or not build. Verify and promote that PR too, or pass allow-overlap
#     after checking by hand that the two are independent (it then only warns).
# and while applying, when a cherry-pick conflicts (the branch is left as it was before
# that PR; nothing is pushed). A PR whose change is already on release is skipped.
# Prints "promoted: <PR> ..." in the order applied.
set -eu

[ $# -ge 4 ] || { echo "usage: $0 BASE DEV PRMAP \"PR ...\" [allow-overlap]" >&2; exit 2; }
base="$1" dev="$2" prmap="$3" chosen="$(printf '%s' "$4" | tr ',' ' ')" allow="${5:-}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() { echo "::error::$*" >&2; failed=1; }
failed=0

# Dev's own line of commits since release (first parent: one commit per PR).
git rev-list --reverse --first-parent "$base..$dev" > "$work/line"

pr_of() { awk -v s="$1" '$1 == s { print $2; exit }' "$prmap"; }
state_of() { awk -v s="$1" '$1 == s { print $3; exit }' "$prmap"; }
sha_of() { awk -v p="$1" '$2 == p { print $1; exit }' "$prmap"; }
is_chosen() { for c in $chosen; do [ "$c" = "$1" ] && return 0; done; return 1; }
files_of() { git diff --name-only "$1^1" "$1"; }

# 1. Every chosen PR is merged into dev, on dev's line, verified and not yet shipped.
for pr in $chosen; do
  case "$pr" in *[!0-9]*|"") fail "\"$pr\" is not a PR number"; continue ;; esac
  sha="$(sha_of "$pr")"
  if [ -z "$sha" ]; then fail "#$pr is not a PR merged into dev"; continue; fi
  if ! grep -qx "$sha" "$work/line"; then
    fail "#$pr ($sha) is not on dev's line since release (already in release, or not on dev)"
    continue
  fi
  case "$(state_of "$sha")" in
    shipped) fail "#$pr is already labelled shipped" ;;
    verified) ;;
    *) fail "#$pr is not labelled verified-on-dev" ;;
  esac
done
[ "$failed" = 0 ] || exit 1
[ -n "$chosen" ] || { echo "::error::no PRs to promote" >&2; exit 1; }

# 2. Dependencies: walk dev's line in order. Files touched by earlier commits that stay
# behind (not chosen, not shipped) are "held"; a chosen commit touching a held file may
# depend on that commit.
: > "$work/held"   # lines: <file>\t<who>
: > "$work/order"
while read -r sha; do
  pr="$(pr_of "$sha")"
  if [ -n "$pr" ] && is_chosen "$pr"; then
    echo "$sha $pr" >> "$work/order"
    files_of "$sha" | sort -u > "$work/files"
    hits="$(awk -F '\t' 'NR == FNR { f[$1] = 1; next } ($1 in f) { print $2 " (" $1 ")" }' \
      "$work/files" "$work/held" | sort -u)"
    if [ -n "$hits" ]; then
      who="$(printf '%s\n' "$hits" | sed 's/ (.*//' | sort -u | tr '\n' ' ')"
      msg="#$pr touches files changed earlier on dev by $who-- not verified/shipped, so #$pr may depend on it: $(printf '%s\n' "$hits" | tr '\n' ';' | sed 's/;$//')"
      if [ "$allow" = allow-overlap ]; then echo "::warning::$msg" >&2; else fail "$msg"; fi
    fi
  elif [ "$(state_of "$sha")" != shipped ]; then
    if [ -n "$pr" ]; then who="#$pr"; else who="commit $(git rev-parse --short "$sha") (no PR)"; fi
    files_of "$sha" | awk -v w="$who" '{ print $0 "\t" w }' >> "$work/held"
  fi
done < "$work/line"
if [ "$failed" != 0 ]; then
  echo "Nothing was cherry-picked. Verify and include the PRs named above, or rerun with allow-overlap once you've checked they are independent." >&2
  exit 1
fi

# 3. Apply in dev order. -x records the dev commit in each message.
applied=""
while read -r sha pr; do
  parents="$(git rev-list --parents -n 1 "$sha" | wc -w)"
  mainline=""
  [ "$parents" -gt 2 ] && mainline="-m 1"   # an older PR merged with a merge commit
  # shellcheck disable=SC2086
  if git cherry-pick -x $mainline "$sha" > "$work/out" 2>&1; then
    applied="$applied $pr"
    echo "#$pr: applied $(git rev-parse --short HEAD) (from $(git rev-parse --short "$sha"))"
    continue
  fi
  conflicts="$(git diff --name-only --diff-filter=U | tr '\n' ' ')"
  if [ -z "$conflicts" ] && git diff --cached --quiet; then
    git cherry-pick --skip >/dev/null 2>&1 || git cherry-pick --abort
    echo "#$pr: already on release; skipped"
    continue
  fi
  git cherry-pick --abort
  cat "$work/out" >&2
  echo "::error::#$pr does not apply on release: conflicts in $conflicts-- it likely needs an earlier dev change that release doesn't have" >&2
  exit 1
done < "$work/order"

echo "promoted:$applied"
