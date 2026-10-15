#!/bin/sh
# Offline test of version.sh against a throwaway repository (CI's Version job runs it
# before the real check):
#
#   sh scripts/version-test.sh
#
# dev gets squash commits with and without changesets, two in one commit, a major below
# 1.0, a dev-v tag in the middle, and release a merge commit of a dev snapshot.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q -b dev
git config user.email test@example.com
git config user.name test
git config commit.gpgsign false
git config tag.gpgsign false
mkdir -p scripts changes crates/app
cp "$here/version.sh" scripts/
v="sh scripts/version.sh"

printf '[workspace.package]\nversion = "0.1.0"\n' > Cargo.toml
git add -A && git commit -qm "start"
change() { # slug level note
  printf 'level: %s\nnote: %s\n' "$2" "$3" > "changes/$1.md"
}
commit() { git add -A && git commit -qm "$1" && git rev-parse HEAD; }

pass=0
ok() { pass=$((pass + 1)); echo "ok - $1"; }
fail() { echo "FAIL - $1" >&2; exit 1; }
expect_eq() { [ "$2" = "$3" ] && ok "$1" || fail "$1: expected '$3', got '$2'"; }
# name, files, expected exit (0/1), pattern in the output
expect_check() {
  out="$(printf '%s\n' $2 | $v check 2>&1)" && got=0 || got=1
  [ "$got" = "$3" ] || fail "$1: exit $got, expected $3: $out"
  case "$out" in *"$4"*) ok "$1" ;; *) fail "$1: expected \"$4\" in: $out" ;; esac
}

expect_eq "no changesets: Cargo.toml's version" "$($v at)" 0.1.0

change versioning minor "Versions on dev."
echo a > crates/app/a.rs
c1="$(commit "versioning")"
expect_eq "a minor changeset" "$($v at)" 0.2.0
expect_eq "its note" "$($v notes)" "Versions on dev."

echo b > crates/app/a.rs
c2="$(commit "no changeset (dependabot)")"
expect_eq "a commit without a changeset keeps the version" "$($v at)" 0.2.0

change fix-a patch "Fix A."
change fix-b minor "Feature B."
c3="$(commit "two changesets")"
expect_eq "the highest level of a commit's changesets, once" "$($v at)" 0.3.0

git tag dev-v0.3.0 "$c3"
change fix-c patch "Fix C."
c4="$(commit "patch after a tag")"
expect_eq "from the nearest dev-v tag" "$($v at)" 0.3.1
git tag -d dev-v0.3.0 > /dev/null
expect_eq "the same without the tag" "$($v at)" 0.3.1
git tag dev-v0.3.0 "$c3"

change breaking major "Breaking D."
c5="$(commit "major below 1.0")"
expect_eq "major below 1.0 bumps the minor" "$($v at)" 0.4.0
expect_eq "an older commit" "$($v at "$c2")" 0.2.0
expect_eq "the first versioned commit" "$($v at "$c1")" 0.2.0

git rm -q changes/fix-a.md
echo c > crates/app/a.rs
commit "deleting an old changeset" > /dev/null
expect_eq "deleting a changeset changes nothing" "$($v at)" 0.4.0

expect_eq "changelog" "$($v changelog | tr '\n' '|')" \
  "## 0.4.0|- Breaking D.||## 0.3.1|- Fix C.||## 0.3.0|- Fix A.|- Feature B.||## 0.2.0|- Versions on dev.||"

git checkout -q -b release "$c1"
git merge -q --no-ff -m "release" "$c4"
expect_eq "a release merge has the dev commit's version" "$($v at)" 0.3.1
git checkout -q dev

git tag dev-v1.4.0 "$c5"
change next-major major "Breaking E."
commit "major at 1.x" > /dev/null
expect_eq "major at 1.x" "$($v at)" 2.0.0
git tag -d dev-v1.4.0 > /dev/null
git tag dev-v0.4 "$c5"
out="$($v at 2>&1)" && fail "a bad tag: $out" || ok "a bad dev-v tag is refused"
git tag -d dev-v0.4 > /dev/null

change pr-ok patch "Fixes the thing."
expect_check "code with a changeset" "crates/app/a.rs changes/pr-ok.md" 0 "changes/pr-ok.md(patch)"
expect_check "code without one" "docs/x.md crates/app/a.rs" 1 "adds no changeset"
expect_check "workflow without one" ".github/workflows/ci.yml" 1 "adds no changeset"
expect_check "Cargo.lock without one" "Cargo.lock" 1 "adds no changeset"
expect_check "deploy without one" "deploy/scripts/deploy.sh" 1 "adds no changeset"
expect_check "docs only" "docs/versioning.md CLAUDE.md scripts/x.sh evals/README.md" 0 "No code changed"
expect_check "a deleted changeset doesn't count" "crates/app/a.rs changes/gone.md" 1 "adds no changeset"
MARGIN_VERSION_EXEMPT=1 expect_check "dependabot" "Cargo.lock Cargo.toml" 0 "Dependabot"
printf 'level: huge\nnote: x\n' > changes/pr-bad.md
expect_check "a bad level" "crates/app/a.rs changes/pr-bad.md" 1 "changes/pr-bad.md needs"
printf 'level: patch\n' > changes/pr-bad.md
expect_check "no note" "crates/app/a.rs changes/pr-bad.md" 1 "needs"
MARGIN_VERSION_EXEMPT=1 expect_check "dependabot's changeset is checked too" "changes/pr-bad.md" 1 "needs"

echo "$pass passed"
