# Margin

Follow `.rules` (the coding, test and PR rules every agent reads; `AGENTS.md` links to it):

@.rules

Margin is the knowledge base for building Margin (`docs/plan-dogfood.md`); the `margin` MCP server (`.mcp.json`, `cargo install --path crates/mcp`, then `margin-mcp token <mga_…>` with an agent token from Settings → Organisation → Agents; `margin-mcp whoami` checks it) is the way in.

- Search Margin (`search_context`) before reading docs for context; read around hits with `document_outline` and `read_passages`, and cite what you use.
- Record each design decision with `ingest_document` (`kind: "decision"`) when it is made.
- The session summary is saved to Margin automatically when a session ends (`SessionEnd` hook, `scripts/claude-session-summary.sh`); run `/margin-handoff` for a fuller hand-written handoff with the session's decisions.
