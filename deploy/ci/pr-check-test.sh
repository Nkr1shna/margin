#!/bin/sh
# Offline test of pr-check.sh (no network, no GitHub):
#
#   sh deploy/ci/pr-check-test.sh
#
# Bodies: the empty template, a filled one with the attribution lines first and the
# generated-by line last, a missing Testing section, an empty Testing section, missing or
# placeholder release notes, bold headings. Sizes: under the budget, over the warning,
# over the limit with and without `large-pr`, Cargo.lock and generated files not counted.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/pr-check.sh"
template="$here/../../.github/pull_request_template.md"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
unset GITHUB_ACTIONS
failures=0

expect() {
  want="$1"
  name="$2"
  shift 2
  if out="$(sh "$script" "$@" 2>&1)"; then got=pass; else got=fail; fi
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: wanted $want, got $got"
    printf '%s\n' "$out" | sed 's/^/     /'
    failures=$((failures + 1))
  fi
  last="$out"
}

body() {
  cat > "$tmp/$1.md"
  echo "$tmp/$1.md"
}

expect fail "the empty template" body "$template"

expect pass "a filled template with attribution and generated-by lines" body "$(body filled <<'EOF'
<!-- claude-projects-attribution: {"github_login":"Nkr1shna"} -->
_Requested by **Krishna**_
<!-- Agents: the two attribution lines from your brief go first. -->
## Summary

**Before:** x. **After:** y.

Closes #12

## Testing

<!-- guidance -->
`sh deploy/local/linux-check.sh`: 412 passed.

Release Notes:

- Fixed the sign-in screen forgetting the service address.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"

expect pass "CRLF line endings" body "$(printf '## Testing\r\n\r\nRan it.\r\n\r\nRelease Notes:\r\n\r\n- N/A\r\n' | body crlf)"

expect pass "bold Release Notes heading and a multi-line comment" body "$(body bold <<'EOF'
## Summary
<!-- a comment
over lines -->
## Testing
Docs only.

**Release Notes:**
- N/A
EOF
)"

expect fail "no Testing section" body "$(body no-testing <<'EOF'
## Summary
Things.

Release Notes:

- N/A
EOF
)"
case "$last" in *"## Testing"*) ;; *) echo "FAIL the error names the Testing section"; failures=$((failures + 1)) ;; esac

expect fail "a Testing section with only a comment" body "$(body empty-testing <<'EOF'
## Testing

<!-- The command you ran -->

## Release Notes:

- N/A
EOF
)"

expect fail "no release notes" body "$(body no-notes <<'EOF'
## Testing
Ran the tests.
EOF
)"

expect fail "release notes without a bullet" body "$(body no-bullet <<'EOF'
## Testing
Ran the tests.

Release Notes:

Nothing user-facing.
EOF
)"

expect fail "the placeholder bullet" body "$(body placeholder <<'EOF'
## Testing
Ran the tests.

Release Notes:

- N/A or Added/Fixed/Improved ...
EOF
)"

tab="$(printf '\t')"
printf 'crates/service/src/a.rs%s400\ndocs/x.md%s100\n' "$tab" "$tab" > "$tmp/small.tsv"
expect pass "500 lines" size "$tmp/small.tsv"

printf 'crates/service/src/a.rs%s1500\n' "$tab" > "$tmp/medium.tsv"
expect pass "1,500 lines passes" size "$tmp/medium.tsv"
case "$last" in *warning*) ;; *) echo "FAIL 1,500 lines warns"; failures=$((failures + 1)) ;; esac

printf 'crates/service/src/a.rs%s2000\ncrates/app/src/b.rs%s1500\n' "$tab" "$tab" > "$tmp/large.tsv"
expect fail "3,500 lines" size "$tmp/large.tsv"
expect fail "3,500 lines with another label" size "$tmp/large.tsv" bug
expect pass "3,500 lines with large-pr" size "$tmp/large.tsv" bug large-pr

printf 'Cargo.lock%s9000\nevals/prompts.lock%s50\nevals/recorded/reranks.jsonl%s4000\nevals/baseline.json%s800\ncrates/app/icon/build/Assets.car%s8000\nCargo.toml%s3\n' \
  "$tab" "$tab" "$tab" "$tab" "$tab" "$tab" > "$tmp/generated.tsv"
expect pass "Cargo.lock and generated files don't count" size "$tmp/generated.tsv"
case "$last" in *": 3"*) ;; *) echo "FAIL counted 3 lines: $last"; failures=$((failures + 1)) ;; esac

printf 'crates/a.rs%s\n' "$tab" > "$tmp/bad.tsv"
expect fail "a file without a count" size "$tmp/bad.tsv"

if [ "$failures" = 0 ]; then
  echo "All pr-check tests passed."
else
  echo "$failures pr-check test(s) failed."
  exit 1
fi
