#!/bin/sh
# A pull request's body and size, checked by .github/workflows/pr-checks.yml (and by hand):
#
#   deploy/ci/pr-check.sh body BODY_FILE
#   deploy/ci/pr-check.sh size FILES_TSV
#   deploy/ci/pr-check.sh title TITLE
#
# body: the body has a `## Testing` section with something in it and a `Release Notes:`
# line followed by at least one bullet, neither left as the template's placeholder.
# HTML comments (the template's guidance, the attribution marker) don't count, and a heading
# inside a fenced code block isn't one: fenced lines are content of the section around them.
#
# size: FILES_TSV holds `path<TAB>changed lines` per file. Lines outside Cargo.lock and
# generated files are counted: over 1,000 warns, over 3,000 fails. No label or override
# lifts the limit.
#
# title: no conventional-commit prefix (`fix:`), no trailing period.
#
# Offline test: sh deploy/ci/pr-check-test.sh
set -eu

warn_lines=1000
max_lines=3000

error() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$1"; else echo "error: $1"; fi
}
warning() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::warning::$1"; else echo "warning: $1"; fi
}

check_body() {
  # Prints `testing <non-blank lines>`, `notes <seen 0|1>`, then one `bullet <text>` line
  # per release-notes bullet.
  summary="$(tr -d '\r' < "$1" | awk '
    {
      line = $0
      out = ""
      while (line != "") {
        if (in_comment) {
          end = index(line, "-->")
          if (end == 0) { line = ""; break }
          line = substr(line, end + 3)
          in_comment = 0
        } else {
          start = index(line, "<!--")
          if (start == 0) { out = out line; line = ""; break }
          out = out substr(line, 1, start - 1)
          line = substr(line, start + 4)
          in_comment = 1
        }
      }
      fence = out ~ /^[ \t]*(```|~~~)/
      if (fence) in_fence = !in_fence
      if (fence || in_fence) {
        if (section == "testing") testing++
        if (section == "notes") section = ""
        next
      }
      lower = tolower(out)
      if (lower ~ /^[ \t]*(#+[ \t]*)?(\*\*)?release notes:?(\*\*)?:?[ \t]*$/) {
        section = "notes"; notes = 1; next
      }
      if (lower ~ /^[ \t]*#+[ \t]*testing[ \t]*:?[ \t]*$/) { section = "testing"; next }
      if (lower ~ /^[ \t]*#+[ \t]/) { section = ""; next }
      if (out ~ /^[ \t]*$/) next
      if (section == "testing") testing++
      if (section == "notes") {
        if (out ~ /^[ \t]*[-*][ \t]/) {
          sub(/^[ \t]*[-*][ \t]+/, "", out)
          bullets[++count] = out
        } else {
          section = ""
        }
      }
    }
    END {
      print "testing " testing + 0
      print "notes " notes + 0
      for (i = 1; i <= count; i++) print "bullet " bullets[i]
    }
  ')"
  ok=1
  if [ "$(printf '%s\n' "$summary" | sed -n 's/^testing //p')" = 0 ]; then
    error "The PR body needs a \`## Testing\` section saying what you ran and what it showed (the command and the pass count, or the manual steps)."
    ok=0
  fi
  if [ "$(printf '%s\n' "$summary" | sed -n 's/^notes //p')" = 0 ]; then
    error "The PR body needs a \`Release Notes:\` section with one bullet: \`- Added/Fixed/Improved ...\` for a change people notice, or \`- N/A\`."
    ok=0
  else
    bullets="$(printf '%s\n' "$summary" | sed -n 's/^bullet //p')"
    if [ -z "$bullets" ]; then
      error "\`Release Notes:\` has no bullet. Add \`- Added/Fixed/Improved ...\` or \`- N/A\`."
      ok=0
    elif printf '%s\n' "$bullets" | grep -Eqi '^[[:space:]]*$|^(N/A or )?Added/Fixed/Improved[[:space:]]*(\.\.\.|…)?[[:space:]]*$'; then
      error "\`Release Notes:\` still has the template's placeholder. Replace it with what changed for the people using Margin, or \`- N/A\`."
      ok=0
    fi
  fi
  [ "$ok" = 1 ]
}

is_generated() {
  case "$1" in
    Cargo.lock|*/Cargo.lock|evals/prompts.lock|evals/baseline.json|evals/recorded/*|crates/app/icon/build/*) return 0 ;;
  esac
  return 1
}

check_size() {
  total=0
  tab="$(printf '\t')"
  while IFS="$tab" read -r path changes; do
    [ -n "$path" ] || continue
    is_generated "$path" && continue
    case "$changes" in ''|*[!0-9]*) error "no changed-line count for $path"; return 1 ;; esac
    total=$((total + changes))
  done < "$1"
  echo "Changed lines (without Cargo.lock and generated files): $total"
  if [ "$total" -gt "$max_lines" ]; then
    error "$total changed lines is over the $max_lines-line limit. Split it into smaller PRs, each tested on its own and linked to its Linear issue (\`Part of ENG-n\`)."
    return 1
  fi
  if [ "$total" -gt "$warn_lines" ]; then
    warning "$total changed lines, over $warn_lines. Smaller PRs are easier to review and revert; consider a split."
  fi
}

# Zed's title rules: no conventional-commit prefix, no trailing period. An area scope
# (`service: ...`) is fine.
check_title() {
  ok=1
  if printf '%s\n' "$1" | grep -Eqi '^(feat|fix|docs|style|refactor|perf|test|chore|build|revert)(\([^)]*\))?!?:'; then
    error "The PR title starts with a conventional-commit prefix. Write an imperative title, optionally scoped by area: \`service: Retry the export when the share is busy\`."
    ok=0
  fi
  case "$1" in
    *.) error "The PR title ends with a period."; ok=0 ;;
  esac
  [ "$ok" = 1 ]
}

case "${1:-}" in
  body) [ $# = 2 ] || { echo "usage: pr-check.sh body BODY_FILE" >&2; exit 2; }; check_body "$2" ;;
  size) [ $# = 2 ] || { echo "usage: pr-check.sh size FILES_TSV" >&2; exit 2; }; check_size "$2" ;;
  title) [ $# = 2 ] || { echo "usage: pr-check.sh title TITLE" >&2; exit 2; }; check_title "$2" ;;
  *) echo "usage: pr-check.sh body BODY_FILE | size FILES_TSV | title TITLE" >&2; exit 2 ;;
esac
