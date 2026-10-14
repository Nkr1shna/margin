# Margin

Margin is the knowledge base for building Margin (`docs/plan-dogfood.md`); the `margin` MCP server (`.mcp.json`, `cargo install --path crates/mcp`, then `margin-mcp login`) is the way in.

- Search Margin (`search_context`) before reading docs for context; read around hits with `document_outline` and `read_passages`, and cite what you use.
- Record each design decision with `ingest_document` (`kind: "decision"`) when it is made.
- Run `/margin-handoff` before ending a build session.
