#!/bin/bash
# Claude Code SessionEnd hook (`.claude/settings.json`): writes this session's summary
# into Margin as a `handoff` document, actor `agent:claude-code`
# (`docs/plan-dogfood.md` §1). Built from facts only, no model call: the commits made
# during the session, what is changed and not committed, and the session's last answer.
#
# Never fails the session: with no `margin-mcp`, no sign-in or no service, it says why
# on stderr and exits 0. Reads the hook's JSON on stdin (`session_id`,
# `transcript_path`, `cwd`). `MARGIN_SUMMARY_DRY_RUN=1` prints the summary instead.
set -u

input=$(cat)
command -v python3 >/dev/null 2>&1 || { echo "margin summary: no python3" >&2; exit 0; }
if [ "${MARGIN_SUMMARY_DRY_RUN:-}" != 1 ] && ! command -v margin-mcp >/dev/null 2>&1; then
  echo "margin summary: margin-mcp isn't installed (cargo install --path crates/mcp)" >&2
  exit 0
fi

summary=$(HOOK_INPUT="$input" python3 -I - <<'PY'
import datetime, json, os, subprocess, sys

def git(*args, cwd):
    try:
        return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True,
                              timeout=20).stdout.strip()
    except Exception:
        return ""

try:
    hook = json.loads(os.environ.get("HOOK_INPUT") or "{}")
except ValueError:
    sys.exit(0)
cwd = hook.get("cwd") or os.getcwd()
session = (hook.get("session_id") or "unknown")[:8]
start, last_text = None, ""
try:
    with open(hook.get("transcript_path") or "", encoding="utf-8") as f:
        for line in f:
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            start = start or entry.get("timestamp")
            if entry.get("type") != "assistant":
                continue
            content = (entry.get("message") or {}).get("content") or []
            texts = [c.get("text", "") for c in content
                     if isinstance(c, dict) and c.get("type") == "text"]
            if any(t.strip() for t in texts):
                last_text = "\n".join(texts).strip()
except OSError:
    pass

branch = git("rev-parse", "--abbrev-ref", "HEAD", cwd=cwd) or "(no branch)"
commits = git("log", f"--since={start}", "--format=%h %s", cwd=cwd).splitlines() if start else []
status = git("status", "--short", "--untracked-files=no", cwd=cwd).splitlines()
if not commits and not status and not last_text:
    sys.exit(0)

today = datetime.date.today().isoformat()
out = [f"# {today} {branch} session {session}", ""]
out += ["## Commits", ""] + ([f"- {c}" for c in commits[:15]] or ["- none"]) + [""]
if status:
    out += ["## Not committed", ""] + [f"- `{s.strip()}`" for s in status[:10]] + [""]
if last_text:
    lines = last_text.splitlines()
    room = max(5, 58 - len(out))
    out += ["## Where the session ended", ""] + lines[:room]
    if len(lines) > room:
        out.append("…")
print(f"{today} {branch} session {session}")
print("\n".join(out[:60]))
PY
) || exit 0
[ -n "$summary" ] || exit 0
name=$(printf '%s\n' "$summary" | head -1)
body=$(printf '%s\n' "$summary" | tail -n +2)

if [ "${MARGIN_SUMMARY_DRY_RUN:-}" = 1 ]; then
  printf 'name: %s\n%s\n' "$name" "$body"
  exit 0
fi
# Bounded, so a service that doesn't answer never holds the session's end.
if ! printf '%s\n' "$body" | python3 -I -c '
import subprocess, sys
try:
    r = subprocess.run(["margin-mcp", "ingest", "--name", sys.argv[1], "--kind", "handoff", "-"],
                       stdin=sys.stdin, capture_output=True, timeout=20)
    sys.exit(r.returncode)
except Exception:
    sys.exit(1)
' "$name"; then
  echo "margin summary: not saved (service or sign-in unavailable)" >&2
fi
exit 0
