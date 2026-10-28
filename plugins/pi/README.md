# pi / omp adapter (not yet implemented)

Planned: an extension calling the generic adapter on prompt submit and agent turn end:

```bash
printf '%s' "$PROMPT" | speedrun ingest prompt --host pi --session "$SESSION_ID"
speedrun ingest turn-end --host pi --session "$SESSION_ID"
```
