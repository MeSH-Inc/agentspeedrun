# agentspeedrun for OpenCode

This plugin supports OpenCode v2; it was exercised against v2.2.5. It records human prompts, ends agent turns when session execution completes, and exposes the speedrun milestone tools. Child session prompts are excluded. OpenCode's prompt admission hook does not expose the resolved model, so the prompt event's model is unset.

```bash
speedrun setup opencode            # user-wide: ~/.config/opencode/plugins/
speedrun setup opencode --project  # this repo: .opencode/plugins/
speedrun init
```

The plugin has no runtime package dependencies. It calls `speedrun` from `PATH`; set `SPEEDRUN_BIN` to override. Set `SPEEDRUN_DEBUG=1` to diagnose failures.

For development, run `cargo build` at the repository root, then `npx tsc --noEmit && bun test` in this directory.

Restart OpenCode after upgrading the adapter. Active-turn tracking is in memory;
hot-reloading the plugin during a turn is not covered by the live checks and
does not transfer that tracking to the new plugin instance. Cleanup does not
invent a completed turn when a reload happens.
