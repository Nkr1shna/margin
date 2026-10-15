#!/bin/sh
# Offline test of release-snapshot.sh and releases-ledger.sh against a throwaway
# repository and a fake `gh` (no network, no GitHub):
#
#   sh deploy/scripts/release-snapshot-test.sh
#
# dev holds an old merge-commit PR, squash commits with passing, failing and missing
# smoke-dev results, docs- and app-only commits (no status of their own; they take the
# nearest service commit's), a service change not yet deployed,
# and a commit with no PR. Checks: the merge-base, snapshot selection
# (default and by sha, with each refusal), the PR list, the notes, the release check for a
# merge commit / fast-forward / squash, a second promotion, and the generated ledger.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/release-snapshot.sh"
ledger="$here/releases-ledger.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Fake gh: `gh api repos/<repo>/commits/<sha>/{status,pulls}` answers from $FAKE/<kind>/<sha>.
mkdir -p "$tmp/bin" "$tmp/fake/status" "$tmp/fake/pulls"
cat > "$tmp/bin/gh" <<'EOF'
#!/bin/sh
[ "$1" = api ] || { echo "fake gh: unexpected $*" >&2; exit 1; }
sha="$(printf '%s' "$2" | sed -n 's|^repos/[^/]*/[^/]*/commits/\([0-9a-f]*\)/.*|\1|p')"
case "$2" in
  */status) cat "$FAKE/status/$sha" 2>/dev/null || echo '{"statuses":[]}' ;;
  */pulls) cat "$FAKE/pulls/$sha" 2>/dev/null || echo '[]' ;;
  *) echo "fake gh: unexpected path $2" >&2; exit 1 ;;
esac
EOF
chmod +x "$tmp/bin/gh"
export PATH="$tmp/bin:$PATH" FAKE="$tmp/fake" REPO=test/repo

pass=0
ok() { pass=$((pass + 1)); echo "ok - $1"; }
fail() { echo "FAIL - $1" >&2; exit 1; }
expect_eq() { [ "$2" = "$3" ] && ok "$1" || fail "$1: expected '$3', got '$2'"; }
expect_refusal() { # name, pattern, command...
  name="$1" pat="$2"; shift 2
  if out="$("$@" 2>&1)"; then fail "$name: expected a refusal, got: $out"; fi
  printf '%s' "$out" | grep -q "$pat" && ok "$name" || fail "$name: refusal didn't mention '$pat': $out"
}
smoke() { printf '{"statuses":[{"context":"ci/other","state":"success"},{"context":"smoke-dev","state":"%s"}]}\n' "$2" > "$FAKE/status/$1"; }
pr() { # sha, number, title, labels (comma-separated), [base]
  labels="$(jq -nc --arg s "$4" '$s | split(",") | map(select(. != "") | {name: .})')"
  jq -n --argjson n "$2" --arg t "$3" --argjson l "$labels" --arg b "${5:-dev}" \
    '[{number: $n, title: $t, labels: $l, base: {ref: $b}, merged_at: "2026-10-14T00:00:00Z"},
      {number: 999, title: "into another branch", labels: [], base: {ref: "main"}, merged_at: "2026-10-14T00:00:00Z"}]' \
    > "$FAKE/pulls/$1"
}
commit() { echo "$1" >> "$1.txt"; git add -A; git commit -q -m "$2"; git rev-parse HEAD; }
commit_at() { mkdir -p "$(dirname "$1")"; echo x >> "$1"; git add -A; git commit -q -m "$2"; git rev-parse HEAD; }
snap() { sh "$script" "$@"; }

cd "$tmp"
git init -q -b release repo
cd repo
git config user.name test
git config user.email test@example.invalid
git config commit.gpgsign false
git config tag.gpgsign false

R0="$(commit base 'release starting point')"
git checkout -q -b dev
# PR #1, merged with a merge commit before squash merging was the rule.
git checkout -q -b feature
side="$(commit one 'PR 1 work')"
git checkout -q dev
git merge -q --no-ff -m 'Merge pull request #1' feature
M1="$(git rev-parse HEAD)"; pr "$M1" 1 'Old merge-commit PR' ''
S2="$(commit two 'Flagged feature (#2)')";  pr "$S2" 2 'Flagged feature' 'needs-flag,verified-on-dev'; smoke "$S2" success
S3="$(commit three 'Broken thing (#3)')";   pr "$S3" 3 'Broken thing' '';                           smoke "$S3" failure
S4="$(commit four 'Docs pushed without a PR')"
S5="$(commit five 'Fix the broken thing (#5)')"; pr "$S5" 5 'Fix the | broken thing' '';           smoke "$S5" success
S6="$(commit six 'Another regression (#6)')"; pr "$S6" 6 'Another regression' '';                 smoke "$S6" error
S7="$(commit_at crates/app/src/x.rs 'App-only change (#7)')"; pr "$S7" 7 'App-only change' ''

# --- First promotion: release has no dev commit beyond its starting point.
expect_eq "base is release's starting point" "$(snap base release dev)" "$R0"
expect_eq "default picks the newest green commit (skips red and unsmoked)" "$(snap select "$R0" dev 2>/dev/null)" "$S5"
skipped="$(snap select "$R0" dev 2>&1 >/dev/null)"
printf '%s' "$skipped" | grep -q "failed on $(git rev-parse --short=7 "$S6") — dev was red at that commit, and $(git rev-parse --short=7 "$S7") changes nothing in the service since" \
  && printf '%s' "$skipped" | grep -q "smoke-dev passed on $(git rev-parse --short=7 "$S5")" \
  && ok "default names the newer commits it skipped and why" || fail "skip messages: $skipped"
expect_eq "a green sha is accepted" "$(snap select "$R0" dev "$(git rev-parse --short "$S2")" 2>/dev/null)" "$S2"
expect_refusal "a red sha is refused" "smoke-dev failed on $(git rev-parse --short=7 "$S3")" snap select "$R0" dev "$S3"
expect_refusal "an error status counts as red" "smoke-dev failed" snap select "$R0" dev "$S6"
expect_refusal "an app-only sha on a red service commit is refused" "smoke-dev failed on $(git rev-parse --short=7 "$S6")" snap select "$R0" dev "$S7"
expect_refusal "a docs commit on a red service commit is refused" "smoke-dev failed on $(git rev-parse --short=7 "$S3")" snap select "$R0" dev "$S4"
expect_refusal "a commit inside a merge-commit PR is refused" "not a commit on dev's first-parent line" snap select "$R0" dev "$side"
expect_refusal "an unknown sha is refused" "is not a commit" snap select "$R0" dev deadbeef

snap prs "$R0" "$S5" > "$tmp/prs.tsv"
expect_eq "PR list: dev order, first-parent, dev PRs only, commit without PR kept" \
  "$(cut -f1 "$tmp/prs.tsv" | tr '\n' ' ')" "1 2 3 - 5 "
grep -q "^-	Docs pushed without a PR ($(git rev-parse --short=7 "$S4"), no PR)	$" "$tmp/prs.tsv" \
  && ok "a commit with no PR is named by subject and sha" || fail "no-PR line: $(cat "$tmp/prs.tsv")"
expect_eq "labels are listed" "$(awk -F '\t' '$1 == 2 { print $3 }' "$tmp/prs.tsv")" "needs-flag,verified-on-dev"
snap notes "$tmp/prs.tsv" > "$tmp/notes.md"
grep -q '^- #2 Flagged feature — `needs-flag` `verified-on-dev` — \*\*ships dark\*\*' "$tmp/notes.md" \
  && ok "notes mark needs-flag PRs as shipping dark" || fail "notes: $(cat "$tmp/notes.md")"
grep -q '^- #5 Fix the | broken thing$' "$tmp/notes.md" && ok "notes keep titles" || fail "notes: $(cat "$tmp/notes.md")"

# The release PR's branch is the snapshot itself; merging it with a merge commit:
git checkout -q release
git merge -q --no-ff -m 'Merge pull request #20 from promote/s5' "$S5"
REL1="$(git rev-parse HEAD)"
B1="$(snap base release dev)"
expect_eq "after a merge-commit release, base is the snapshot" "$B1" "$S5"
snap verify "$B1" "$REL1" && ok "a merge-commit release is exactly the snapshot" || fail "verify merge commit"
git tag -a "release-2026-10-14-$(git rev-parse --short=7 "$S5")" -m "Release

dev-sha: $S5
previous-dev-sha: $R0
released: 2026-10-14
PRs:
$(cat "$tmp/notes.md")"

# Fast-forward gives the same base; a squash merge is caught.
git branch -q ff "$R0"; git checkout -q ff; git merge -q --ff-only "$S5"
expect_eq "after a fast-forward release, base is the snapshot" "$(snap base ff dev)" "$S5"
snap verify "$(snap base ff dev)" "$(git rev-parse ff)" && ok "a fast-forward release is exactly the snapshot" || fail "verify ff"
git checkout -q -b squashed "$R0"; git merge -q --squash "$S5" > /dev/null; git commit -q -m 'Squashed release PR'
expect_eq "a squash leaves the base behind" "$(snap base squashed dev)" "$R0"
expect_refusal "a squash-merged release is refused" "squash-merged" snap verify "$(snap base squashed dev)" "$(git rev-parse squashed)"
git checkout -q release

# --- Second promotion: only what came after the first snapshot.
expect_refusal "nothing green after the last release" "no dev commit after $(git rev-parse --short=7 "$S5") passed smoke-dev" snap select "$B1" dev
expect_refusal "an already-released sha is refused" "already on release" snap select "$B1" dev "$S2"
smoke "$S7" success
expect_eq "once #7 is green it is the snapshot" "$(snap select "$B1" dev 2>/dev/null)" "$S7"
expect_eq "second PR list starts after the first snapshot" "$(snap prs "$B1" "$S7" | cut -f1 | tr '\n' ' ')" "6 7 "

# --- Commits that don't change the service (docs, app) are never deployed, so they have no
# smoke-dev status of their own: they take the nearest service commit's result.
git checkout -q dev
S8="$(commit_at docs/x.md 'Docs only (#8)')"; pr "$S8" 8 'Docs only' ''
expect_eq "a docs-only commit on a green service commit is the snapshot" "$(snap select "$B1" dev 2>/dev/null)" "$S8"
why="$(snap select "$B1" dev "$S8" 2>&1 >/dev/null)"
printf '%s' "$why" | grep -q "smoke-dev passed on $(git rev-parse --short=7 "$S7"), and $(git rev-parse --short=7 "$S8") changes nothing in the service since" \
  && ok "it says which commit's smoke-dev it relies on" || fail "reason: $why"
S9="$(commit_at crates/service/src/x.rs 'Service change, not deployed yet (#9)')"; pr "$S9" 9 'Service change' ''
expect_refusal "a service change with no status is refused" "$(git rev-parse --short=7 "$S9") changes the service but has no smoke-dev result" snap select "$B1" dev "$S9"
S10="$(commit_at docs/y.md 'Docs on top of it (#10)')"; pr "$S10" 10 'Docs on top' ''
expect_refusal "a docs commit on an undeployed service change is refused" "$(git rev-parse --short=7 "$S9") changes the service but has no smoke-dev result (it was never deployed to dev; $(git rev-parse --short=7 "$S10") builds on it" snap select "$B1" dev "$S10"
expect_eq "default skips both and picks the docs commit on the green one" "$(snap select "$B1" dev 2>/dev/null)" "$S8"
for p in Cargo.lock deploy/x.sh .github/workflows/deploy-dev.yml crates/shared/src/x.rs; do
  s="$(commit_at "$p" "touches $p")"
  expect_refusal "$p counts as a service change" "$(git rev-parse --short=7 "$s") changes the service" snap select "$B1" dev "$s"
done
for p in crates/app/src/y.rs .github/workflows/ci.yml README.md; do
  s="$(commit_at "$p" "touches $p")"
  expect_refusal "$p does not count as a service change" "$(git rev-parse --short=7 "$s") builds on it" snap select "$B1" dev "$s"
done
# Release at #8; a docs commit after it changes nothing prod doesn't already run.
git checkout -q release
git merge -q --no-ff -m 'Merge pull request #21 from promote/s8' "$S8"
git checkout -q dev
git checkout -q -b docs-after "$S8"
S11="$(commit_at docs/z.md 'Docs after the release (#11)')"
expect_eq "a docs-only commit right after the released one is accepted" "$(snap select "$S8" docs-after 2>/dev/null)" "$S11"
snap select "$S8" docs-after 2>&1 >/dev/null | grep -q "changes nothing in the service since $(git rev-parse --short=7 "$S8"), the last dev commit release has" \
  && ok "it says the service is unchanged since the release" || fail "unchanged reason"
expect_refusal "nothing on dev after the base" "dev has nothing after" snap select "$S11" docs-after
git checkout -q dev
git branch -q -D docs-after

# --- Ledger from the tags.
sh "$ledger" > "$tmp/RELEASES.md"
grep -q "^## release-2026-10-14-$(git rev-parse --short=7 "$S5")$" "$tmp/RELEASES.md" && ok "ledger lists the release tag" || fail "ledger: $(cat "$tmp/RELEASES.md")"
grep -q "dev \`$(git rev-parse --short=7 "$S5")\`" "$tmp/RELEASES.md" && ok "ledger names the dev sha" || fail "ledger dev sha"
grep -q '^- #2 Flagged feature' "$tmp/RELEASES.md" && grep -q '^- #5 ' "$tmp/RELEASES.md" && ok "ledger lists the PRs" || fail "ledger PRs"
git tag -d "release-2026-10-14-$(git rev-parse --short=7 "$S5")" > /dev/null
sh "$ledger" | grep -q "No releases yet" && ok "ledger with no tags" || fail "empty ledger"

echo "all $pass checks passed"
