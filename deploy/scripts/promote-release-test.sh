#!/bin/sh
# Dry run of promote-release.sh against a throwaway repository (no network, no GitHub):
#
#   sh deploy/scripts/promote-release-test.sh
#
# Builds a release branch and a dev branch of squash commits (plus one older merge-commit
# PR), a PR map like the one promote-release.yml writes, and checks each rule: dev order,
# refusing unverified/shipped PRs, refusing a dependency on an unverified earlier PR,
# a conflicting cherry-pick, and skipping a change release already has.
set -eu
script="$(cd "$(dirname "$0")" && pwd)/promote-release.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q -b release repo
cd repo
git config user.name test
git config user.email test@example.invalid
git config commit.gpgsign false

commit() { # file content message
  printf '%s\n' "$2" > "$1"
  git add "$1"
  git commit -q -m "$3"
  git rev-parse HEAD
}

commit base.txt base "release" > /dev/null
commit f.txt same "on release already" > /dev/null
git checkout -q -b dev HEAD~1
pr1="$(commit a.txt one "PR 1")"
pr2="$(commit b.txt two "PR 2")"
pr3="$(commit b.txt three "PR 3 builds on PR 2")"
pr4="$(commit c.txt four "PR 4")"
# PR 5: an older PR merged with a merge commit.
git checkout -q -b feature
commit d.txt five "PR 5 work" > /dev/null
git checkout -q dev
git merge -q --no-ff -m "Merge PR 5" feature
pr5="$(git rev-parse HEAD)"
pr6="$(commit e.txt six "PR 6")"
pr8="$(commit f.txt same "PR 8 (release has it)")"
git checkout -q release

cat > "$tmp/prmap" <<EOF
$pr1 1 verified
$pr2 2 pending
$pr3 3 verified
$pr4 4 verified
$pr5 5 verified
$pr6 6 shipped
$pr8 8 verified
EOF

pass=0
expect() { # name want-exit want-text prs [allow]
  name="$1" want="$2" text="$3"
  shift 3
  git checkout -q -B "promote-$name" release
  set +e
  out="$(sh "$script" release dev "$tmp/prmap" "$@" 2>&1)"
  got=$?
  set -e
  if [ "$got" != "$want" ] || ! printf '%s' "$out" | grep -q -- "$text"; then
    echo "FAIL $name: exit $got (want $want), output:"
    printf '%s\n' "$out"
    exit 1
  fi
  pass=$((pass + 1))
  echo "ok   $name"
}

expect order 0 "promoted: 1 4 5" "5,1,4"
[ "$(git log --format=%s release..HEAD | tr '\n' '|')" = "Merge PR 5|PR 4|PR 1|" ] \
  || { echo "FAIL order: wrong commits"; git log --oneline release..HEAD; exit 1; }
[ -f d.txt ] || { echo "FAIL order: merge-commit PR not applied"; exit 1; }
expect unverified 1 "#2 is not labelled verified-on-dev" "2"
expect shipped 1 "#6 is already labelled shipped" "6"
expect unknown 1 "#99 is not a PR merged into dev" "99"
expect depends 1 "#3 touches files changed earlier on dev by #2" "3"
[ "$(git rev-parse HEAD)" = "$(git rev-parse release)" ] || { echo "FAIL depends: branch changed"; exit 1; }
expect conflict 1 "#3 does not apply on release" "3" allow-overlap
[ -z "$(git status --porcelain)" ] || { echo "FAIL conflict: tree left dirty"; exit 1; }
expect skip 0 "#8: already on release; skipped" "8"
echo "$pass checks passed"
