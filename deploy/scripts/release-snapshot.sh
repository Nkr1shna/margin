#!/bin/sh
# Release = a verified dev snapshot (docs/plan-deploy.md §4a). The git and GitHub logic
# behind promote-release.yml, kept here so release-snapshot-test.sh can check it offline.
#
#   release-snapshot.sh base <release-ref> <dev-ref>
#       The last dev commit release contains (git merge-base). After a promotion merged
#       with a merge commit or fast-forward, that is the promoted snapshot.
#   release-snapshot.sh select <base> <dev-ref> [sha]
#       The dev commit to promote: `sha` if given, else the newest commit on dev's
#       first-parent line after <base> whose smoke-dev result is `success`. Refuses, naming
#       the commit, when `sha` isn't a dev commit after <base>, or its smoke-dev result
#       failed or is missing; or when no commit after <base> passed.
#       A commit with no smoke-dev status that doesn't change the service (docs-only,
#       app-only: none of build-service.yml's paths) was never deployed because the
#       service in it is its parent's, so it takes its first parent's result; if that walk
#       reaches <base>, the service is unchanged since the last release and it counts as
#       passed. A commit that changes the service must have its own status.
#   release-snapshot.sh prs <from> <to>
#       The dev PRs merged in <from>..<to> (first-parent, oldest first) as TSV:
#       number, title, labels (comma-separated). A commit with no PR is listed as
#       "-", "<subject> (<short sha>, no PR)", "".
#   release-snapshot.sh notes <prs.tsv>
#       Those PRs as a Markdown list (used in the release PR body and the tag message).
#   release-snapshot.sh verify <base> <release-sha>
#       Succeeds when release's tree is exactly dev's <base> (a merge commit or a
#       fast-forward of a snapshot); fails when it isn't (a squash merge, or a direct push).
#
# smoke-dev records its result as the commit status `smoke-dev` on the deployed dev sha.
# Needs git, jq and gh (signed in, or GH_TOKEN); REPO (or GITHUB_REPOSITORY) names the repo.
set -eu
repo="${REPO:-${GITHUB_REPOSITORY:-Nkr1shna/margin}}"
# How many dev commits `select` checks for a passing smoke-dev before giving up.
max_scan="${SNAPSHOT_MAX_SCAN:-100}"
# What build-service.yml builds and deploys from (keep in step with its `paths:`): a commit
# that changes none of these leaves the running service as its parent's.
service_paths='^(crates/(service|agent|ai|embed|video|shared)/|Cargo\.(toml|lock)$|deploy/|\.github/workflows/(build-service|deploy-dev|deploy-prod)\.yml$)'

die() { echo "refused: $*" >&2; exit 1; }
short() { git rev-parse --short=7 "$1"; }

# success | failure | error | pending | missing (smoke-dev never ran on it). Each commit is
# asked once per run: `select` walks the same commits for several candidates.
states="$(mktemp -d)"
trap 'rm -rf "$states"' EXIT
smoke_state() {
  [ -s "$states/$1" ] || gh api "repos/$repo/commits/$1/status" \
    | jq -r '[.statuses[]? | select(.context == "smoke-dev")][0].state // "missing"' > "$states/$1"
  cat "$states/$1"
}

changes_service() { # sha: does the commit (against its first parent) touch the service?
  git diff-tree --no-commit-id --name-only -r --first-parent "$1" | grep -Eq "$service_paths"
}

# The smoke-dev result that applies to a commit: its own status, or, when it has none and
# doesn't change the service, its first parent's (down to the base). Prints
# "<state> <sha the state came from>"; state `unchanged` means the walk reached the base:
# the service is what release already has.
verdict() { # sha, base
  c="$1"
  while :; do
    state="$(smoke_state "$c")"
    if [ "$state" != missing ] || changes_service "$c"; then echo "$state $c"; return; fi
    c="$(git rev-parse "$c^")"
    if [ "$c" = "$2" ]; then echo "unchanged $c"; return; fi
  done
}

since() { [ "$1" = "$2" ] || printf ', and %s changes nothing in the service since' "$(short "$1")"; }

explain() { # sha, state, from-sha
  case "$2" in
    failure|error) echo "smoke-dev failed on $(short "$3") — dev was red at that commit$(since "$1" "$3")" ;;
    pending) echo "smoke-dev is still running on $(short "$3")$(since "$1" "$3")" ;;
    *) echo "$(short "$3") changes the service but has no smoke-dev result (it was never deployed to dev$( [ "$1" = "$3" ] || printf '; %s builds on it' "$(short "$1")"); run build-service on dev to deploy and smoke-test the tip)" ;;
  esac
}

accepted() { # sha, state, from-sha
  if [ "$2" = unchanged ]; then
    echo "$(short "$1") changes nothing in the service since $(short "$3"), the last dev commit release has"
  else
    echo "smoke-dev passed on $(short "$3")$(since "$1" "$3")"
  fi
}

cmd="${1:-}"
[ $# -gt 0 ] && shift
case "$cmd" in
  base)
    [ $# -eq 2 ] || die "usage: base <release-ref> <dev-ref>"
    git merge-base "$1" "$2" || die "$1 and $2 share no history"
    ;;

  select)
    [ $# -ge 2 ] || die "usage: select <base> <dev-ref> [sha]"
    base="$(git rev-parse --verify -q "$1^{commit}")" || die "unknown commit $1"
    dev="$2"
    cands="$(git rev-list --first-parent "$base..$dev")"
    [ -n "$cands" ] || die "dev has nothing after $(short "$base"), the dev commit release already has"
    if [ -n "${3:-}" ]; then
      sha="$(git rev-parse --verify -q "$3^{commit}")" || die "$3 is not a commit"
      if ! printf '%s\n' "$cands" | grep -qx "$sha"; then
        if git merge-base --is-ancestor "$sha" "$base"; then
          die "$(short "$sha") is already on release (release has dev up to $(short "$base"))"
        fi
        die "$(short "$sha") is not a commit on dev's first-parent line after $(short "$base") (pick a squash commit on dev, not a commit inside a PR)"
      fi
      set -- $(verdict "$sha" "$base")
      case "$1" in success|unchanged) ;; *) die "$(explain "$sha" "$1" "$2")" ;; esac
      accepted "$sha" "$1" "$2" >&2
      echo "$sha"
      exit 0
    fi
    n=0
    for c in $cands; do
      n=$((n + 1))
      [ "$n" -le "$max_scan" ] || break
      set -- $(verdict "$c" "$base")
      case "$1" in
        success|unchanged) accepted "$c" "$1" "$2" >&2; echo "$c"; exit 0 ;;
      esac
      echo "skipping $(explain "$c" "$1" "$2")" >&2
    done
    newest="$(printf '%s\n' "$cands" | head -n 1)"
    set -- $(verdict "$newest" "$base")
    die "no dev commit after $(short "$base") passed smoke-dev (checked $((n > max_scan ? max_scan : n))); newest: $(explain "$newest" "$1" "$2")"
    ;;

  prs)
    [ $# -eq 2 ] || die "usage: prs <from> <to>"
    for c in $(git rev-list --first-parent --reverse "$1..$2"); do
      out="$(gh api "repos/$repo/commits/$c/pulls" | jq -r '
        .[] | select(.base.ref == "dev" and .merged_at != null)
        | [ (.number | tostring), (.title | gsub("[\t\n]"; " ")), ([.labels[].name] | join(",")) ]
        | @tsv')"
      if [ -n "$out" ]; then
        printf '%s\n' "$out"
      else
        printf -- '-\t%s (%s, no PR)\t\n' "$(git log -1 --format=%s "$c" | tr '\t' ' ')" "$(short "$c")"
      fi
    done | awk -F '\t' '$1 == "-" || !seen[$1]++'
    ;;

  notes)
    [ $# -eq 1 ] || die "usage: notes <prs.tsv>"
    [ -s "$1" ] || { echo "- (no PRs)"; exit 0; }
    awk -F '\t' '{
      line = ($1 == "-") ? "- " $2 : "- #" $1 " " $2
      if ($3 != "") {
        n = split($3, l, ",")
        tags = ""
        for (i = 1; i <= n; i++) tags = tags (i > 1 ? " " : "") "`" l[i] "`"
        line = line " — " tags
      }
      if ($3 ~ /(^|,)needs-flag(,|$)/) line = line " — **ships dark**: turn its setting on in prod when ready"
      print line
    }' "$1"
    ;;

  verify)
    [ $# -eq 2 ] || die "usage: verify <base> <release-sha>"
    if ! git diff --quiet "$1" "$2" --; then
      die "release ($(short "$2")) is not exactly a dev commit: its files differ from $(short "$1"), the last dev commit it contains. A release PR was squash-merged, or something was pushed to release directly. Release PRs must be merged with a merge commit."
    fi
    ;;

  *)
    sed -n '2,24p' "$0" >&2
    exit 2
    ;;
esac
