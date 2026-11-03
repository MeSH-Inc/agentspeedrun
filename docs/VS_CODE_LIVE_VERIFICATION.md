# VS Code live agent verification

On 2026-11-03, an actual VS Code 1.144.0 Apple silicon session used the
agentspeedrun 0.3.3 extension loaded from the checkout via
`--extensionDevelopmentPath`, and 0.3.3 debug CLI and daemon binaries. The
project, Speedrun data, daemon address, VS Code profile and extension directory
were isolated. The user signed in to Copilot through its normal browser flow.
The extension was not installed from a VSIX in this check.

In VS Code's **Local** agent harness, Auto selected GPT-6 Luna. A real prompt
asked the agent to call `speedrun_status` and `speedrun_propose_milestones`.
Both MCP calls completed after individual UI approvals, the agent completed its
response, and the Speedrun view showed the requested agent-generated Candidate
milestone. The fresh run contained prompt submission, turn start, proposal, and
turn end; `speedrun verify` reported all nine events signed, and status showed
zero open turns. The recorded prompt's optional model field was unset; the
model is known from the VS Code UI.

The check was repeated using the prerelease VSIX installed in the isolated
extension directory and a normal VS Code window without
`--extensionDevelopmentPath`. The VSIX SHA-256 was
`337bb1cd146fae5de0e51eb6223dc1124742df65796d32d362d7eac2b91edaf6`;
the rebuilt 0.3.3 debug CLI SHA-256 was
`4c18f4a08b2fecd62b90bb1e0dc8daad86d364ff6d0f430c515dc4b958cfd3d3`.
The installed extension activated, and a fresh Local GPT-6 Luna turn completed
both tools after individual approvals and displayed the requested agent
proposal. The fresh run again verified nine signed events, including prompt,
turn start, agent-generated proposal and turn end, with zero open turns.

After reload, a new chat defaulted to the newer **Copilot** harness, which
VS Code identifies as a Copilot SDK agent in the Agent Host. The initial
prompt was canceled while MCP servers were still starting; its absence from
the run was inconclusive. A later live retest with the installed 0.3.3
extension and the same signed-in isolated profile used an instrumented wrapper
around the generated workspace hooks. Auto selected GPT-6 Luna. The SDK
executed `SessionStart`, `UserPromptSubmit`, and `Stop`, each with the same
session ID and the project as both hook cwd and payload cwd. The prompt was
recorded, the stop reason was `end_turn`, and the view stopped showing an
active agent. The prompt callback arrived about ten seconds after session
start, while an unrelated MCP server was starting. This establishes capture
and turn closure in that configuration; it does not explain the earlier
canceled attempt.

Early SDK retests reported both tools unavailable after the operator chose
**Skip** while an unrelated Railway MCP server was starting. After allowing
that server's 60-second timeout to finish, a fresh GPT-6 Luna turn found and
called the extension-provided `agentspeedrun` tools `speedrun_status` and
`speedrun_propose_milestones`. Both calls completed after UI approval, and
the agent reported the requested candidate milestone.

This was repeated with the temporary project `.vscode/mcp.json` and root
`.mcp.json` removed. The extension's `agentspeedrun` server supplied both
tools, which completed after individual approvals. The SDK agent reported
the new candidate, status showed it persisted with no active agent, and the
instrumented hooks recorded `SessionStart`, `UserPromptSubmit`, and `Stop`
with `end_turn`. `speedrun verify` reported all 29 events in the shared test
run signed. The prompt callback in this test arrived about 60 seconds after
session start, following the unrelated server timeout. The earlier skipped
startup produced misleading negative tool results; the cause of the original
canceled prompt's absence remains unconfirmed.

The final candidate VSIX corrects the README installation link. Its SHA-256 is
`b0e8b29efbe1622533264e4bcdf4b1def197b5d42b62d7054019518a83746df1`.
Every ZIP entry except `extension/readme.md` is byte-identical to the GUI-tested
package above. Prerelease metadata and isolated installation, replacement,
listing and removal passed again; the documentation change does not constitute
a new model-turn test.
