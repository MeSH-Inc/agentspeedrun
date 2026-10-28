# opencode adapter (not yet implemented)

Planned: a TypeScript plugin subscribing to opencode's session events that shells out to the generic adapter:

```bash
printf '%s' "$PROMPT" | speedrun ingest prompt --host opencode --session "$SESSION_ID" --model "$MODEL"
speedrun ingest turn-end --host opencode --session "$SESSION_ID"
```

Register `speedrun mcp` as an MCP server in opencode's config for the milestone tools.
