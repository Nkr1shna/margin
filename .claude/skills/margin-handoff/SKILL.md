---
name: margin-handoff
description: End-of-session handoff for the Margin repo. Writes a short summary of this Claude Code session (what changed, why, open items, commit hashes) into Margin, the project's knowledge base, through the margin MCP server's ingest_document tool. Use when the owner runs /margin-handoff or asks to hand off or wrap up a build session.
---

# /margin-handoff

Write this session's handoff into Margin so the next session (and Margin's own agent) can find it.

1. Gather the facts, don't guess them:
   - `git log --oneline <base>..HEAD` on the current branch (the base is the branch this work started from, usually `claude/agent-build`), and `git status --short`.
   - The decisions made this session and the questions still open with the owner, from the conversation.
2. Write the summary in Markdown, **at most 60 lines**, with exactly these sections:
   - `## What changed`: files and behaviour, one line each, with the commit hash.
   - `## Why`: the reasons and the decisions behind them (link to `docs/…` by path where a doc says more).
   - `## Open items`: what is unfinished, what is blocked and on whom, and questions for the owner.
   - `## Commits`: branch name and every commit hash with its subject line.
   No secrets, tokens, keys or personal data; no pasted tool output or transcripts.
3. Call the `ingest_document` tool of the `margin` MCP server with:
   - `name`: `<YYYY-MM-DD> <branch> <short session topic>` (the session id goes in the name, not the actor),
   - `kind`: `"handoff"` (the name becomes `Handoff: …`),
   - `text`: the summary.
   The service records it as `actor: agent:claude-code`.
4. For each design decision made this session, also call `ingest_document` with `kind: "decision"`, `name` = the topic, and a few lines: the decision, the alternatives, why.
5. Tell the owner the document names and ids that came back. If the `margin` server is unavailable (not installed, or `margin-mcp login` not done), say so and print the summary instead of writing it anywhere else.
