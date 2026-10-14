#!/bin/sh
# Keeps Margin's copy of the repository's docs current (docs/plan-dogfood.md §1).
# The body of a `post-commit` hook; install it by hand, never commit it into .git:
#
#   printf '#!/bin/sh\nexec "$(git rev-parse --show-toplevel)/scripts/ingest-docs.sh"\n' > .git/hooks/post-commit
#   chmod +x .git/hooks/post-commit
#
# (In a worktree, the hooks folder is `git rev-parse --git-common-dir`/hooks.)
# Runs only on claude/agent-build, in the background so the commit isn't held up, and
# logs to ~/Library/Logs/margin-ingest-docs.log. Run it by hand for the first ingest.
set -eu

branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
if [ "$branch" != "claude/agent-build" ] && [ "${1:-}" != "--any-branch" ]; then
  exit 0
fi
if ! command -v margin-mcp >/dev/null 2>&1; then
  echo "ingest-docs: margin-mcp isn't on PATH (cargo install --path crates/mcp)" >&2
  exit 0
fi

root="$(git rev-parse --show-toplevel)"
log="$HOME/Library/Logs/margin-ingest-docs.log"
mkdir -p "$(dirname "$log")"
(
  cd "$root"
  {
    echo "--- $(date '+%Y-%m-%d %H:%M:%S') $(git rev-parse --short HEAD)"
    margin-mcp ingest-dir docs PLAN.md || echo "ingest-docs: failed"
  } >>"$log" 2>&1
) &
