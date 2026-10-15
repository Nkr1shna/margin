#!/bin/sh
# Margin's version (docs/versioning.md). A PR that changes code adds a changeset,
# changes/<slug>.md, two lines:
#
#   level: patch|minor|major
#   note: One line for the changelog.
#
# Nothing commits a version: a dev commit's version is computed from git history. Start at
# the nearest dev-v<version> tag on dev's first-parent line (else Cargo.toml's
# [workspace.package] version), then bump once per later commit by the highest level among
# the changesets that commit added. A commit without a changeset keeps its parent's
# version. Below 1.0, major bumps the minor (Cargo's convention). A merge commit (release)
# has the version of the dev commit it merged. tag-version.yml tags each bumped dev commit
# dev-v<version>; the tags only save walking, the answer is the same without them.
#
#   scripts/version.sh at [REV]          the version of REV (default HEAD)
#   scripts/version.sh notes [REV]       the changeset notes REV added, one per line
#   scripts/version.sh changelog [REV]   every version up to REV with its notes, newest first
#   scripts/version.sh check < files     does a PR changing these files (paths, one per
#                                        line, read from this checkout) carry a valid
#                                        changeset? MARGIN_VERSION_EXEMPT=1 (Dependabot) may
#                                        go without one.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() { echo "$*" >&2; exit 1; }

valid() {
  printf '%s\n' "$1" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

# The level a changeset declares (from stdin), or nothing.
level_of() { sed -nE 's/^level: *(patch|minor|major) *$/\1/p' | head -n 1; }
note_of() { sed -n 's/^note: *//p' | head -n 1; }

# patch < minor < major.
rank() { case "$1" in patch) echo 1 ;; minor) echo 2 ;; major) echo 3 ;; *) echo 0 ;; esac; }

bump() { # version level
  echo "$1" | awk -F. -v level="$2" '{
    if (level == "major" && $1 == 0) level = "minor"
    if (level == "patch") print $1 "." $2 "." $3 + 1
    else if (level == "minor") print $1 "." $2 + 1 ".0"
    else print $1 + 1 ".0.0"
  }'
}

# The changesets commit $1 added.
added() {
  git diff-tree --root --no-commit-id --name-only --diff-filter=A -r "$1" -- 'changes/*.md'
}

# The highest level among the changesets commit $1 added, or nothing.
commit_level() {
  best=""
  for f in $(added "$1"); do
    l="$(git show "$1:$f" | level_of)"
    [ "$(rank "$l")" -gt "$(rank "$best")" ] && best="$l"
  done
  echo "$best"
}

# Prints "<sha> <version>" for every first-parent commit from the start point up to $1
# that bumped, oldest first; the first line is the start point. With $2 = all it starts
# from Cargo.toml's version and ignores the tags.
walk() {
  rev="$(git rev-parse --verify -q "$1^{commit}")" || die "no commit $1"
  if [ -n "$(git rev-parse -q --verify "$rev^2" 2>/dev/null)" ]; then
    rev="$(git rev-parse "$rev^2")"
  fi
  if [ "${2:-}" != all ] && tag="$(git describe --tags --abbrev=0 --first-parent --match 'dev-v*' "$rev" 2>/dev/null)"; then
    start="${tag#dev-v}"
    valid "$start" || die "tag $tag isn't dev-vMAJOR.MINOR.PATCH"
    from="$(git rev-parse "$tag^{commit}")"
    range="$from..$rev"
  else
    start="$(git show "$rev:Cargo.toml" | sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p')"
    valid "$start" || die "Cargo.toml's [workspace.package] version \"$start\" isn't MAJOR.MINOR.PATCH"
    from=""
    range="$rev"
  fi
  echo "${from:-start} $start"
  v="$start"
  for c in $(git rev-list --first-parent --reverse "$range" -- changes); do
    l="$(commit_level "$c")"
    [ -n "$l" ] || continue
    v="$(bump "$v" "$l")"
    echo "$c $v"
  done
}

# Each walk goes to a variable first: a pipe would hide its failure.
at() {
  w="$(walk "${1:-HEAD}")"
  printf '%s\n' "$w" | tail -n 1 | cut -d' ' -f2
}

notes() {
  for f in $(added "${1:-HEAD}"); do
    git show "${1:-HEAD}:$f" | note_of
  done
}

changelog() {
  w="$(walk "${1:-HEAD}" all)"
  printf '%s\n' "$w" | sed 1d | sed -n '1!G;h;$p' | while read -r c v; do
    echo "## $v"
    notes "$c" | sed 's/^/- /'
    echo
  done
}

# Does this path change what is built or deployed? (Docs, evals data and agent scripts don't.)
is_code() {
  case "$1" in
    crates/* | Cargo.toml | Cargo.lock | rust-toolchain.toml | deploy/* | .github/*) return 0 ;;
  esac
  return 1
}

check() {
  code="" sets="" bad=""
  while IFS= read -r f; do
    case "$f" in
      changes/*.md)
        # A changeset this PR deleted is not one it adds.
        [ -f "$f" ] || continue
        l="$(level_of < "$f")"
        n="$(note_of < "$f")"
        if [ -z "$l" ] || [ -z "$n" ]; then
          bad="$bad $f"
        else
          sets="$sets $f($l)"
        fi
        ;;
      *) [ -z "$code" ] && is_code "$f" && code="$f" ;;
    esac
  done
  [ -z "$bad" ] || die "changeset$bad needs a \"level: patch|minor|major\" line and a \"note: ...\" line (docs/versioning.md)."
  if [ -n "$sets" ]; then
    echo "Changesets:$sets."
  elif [ -z "$code" ]; then
    echo "No code changed; no changeset needed."
  elif [ "${MARGIN_VERSION_EXEMPT:-}" = 1 ]; then
    echo "Dependabot: no changeset needed."
  else
    die "this PR changes code ($code, ...) but adds no changeset: add changes/<slug>.md with \"level: patch|minor|major\" and \"note: ...\" (docs/versioning.md says which level)."
  fi
}

case "${1:-}" in
  at) at "${2:-}" ;;
  notes) notes "${2:-}" ;;
  changelog) changelog "${2:-}" ;;
  check) check ;;
  *) die "usage: version.sh at [REV] | notes [REV] | changelog [REV] | check < changed-files" ;;
esac
