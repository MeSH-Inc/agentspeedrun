# agentspeedrun end state

The end state is COMMONS **P1**: a published run anyone can believe without trusting the
author's machine. It is reached through v0.2 ([V0.2.md](V0.2.md), which is COMMONS P0). P2
and later are out of scope: they are mostly governance (C2, C5, C7).

This is an index of the acceptance items in `V0.2.md` and `COMMONS.md` §14, which define
them. **Who**: `loop` means a bounded change a test can judge, which an automated proposer
can make once the test exists; `leader` means it needs design, a schema or category change,
or code outside the Rust workspace. **Test**: an ignored objective test that fails until the
item holds (`cargo test --all -- --ignored audit_`), or a test that shows it already holds.

## v0.2 (COMMONS P0)

| Item | Condition | Who | Decision | Test |
|---|---|---|---|---|
| M1.a | `speedrun hook`/`ingest` exit 0 on any argument error | loop | | `audit_a4_1_*` |
| M1.b | The marketplace plugin source is pinned to release tags | leader | D1 | |
| M2 | Token creation follows the single-instance bind; the client spools on 401, 5xx, timeout | leader | | |
| M3 | Daemon logs reach stderr or a capped file | loop | | |
| M4 | SIGTERM with an SSE stream open ends the daemon within about 2 s | leader | | |
| M5.a | Agent work while paused counts; a prompt or turn start resumes the run; pausing with an open turn is rejected | loop | | `audit_a6_1_*` |
| M5.b | v0.1 PBs whose value changes under M5 are labelled | leader | D4 | |
| M6.a | `git_commit` evidence from before run start does not verify | loop | | `audit_a3_1_*` |
| M6.b | A `ci_run` whose head commit predates run start does not verify | loop | | |
| M7.a | Redaction covers the shapes listed in M7 | loop | | `audit_a1_2_*` |
| M7.b | The client redacts before sending or spooling; `rejected.jsonl` is capped | loop | | |
| M8 | A hook records its own event first and flushes the spool within about 3 s | leader | | |
| M9.a | A user's claim satisfies `agent_claimed` on a non-predefined milestone | loop | | `audit_a6_5_*` |
| M9.b | `accept` can set `required_trust` | loop | | |
| M10 | The docs claim only what the code does | leader | | |
| W1.a | `speedrun verify` passes on a frozen v0.1 fixture DB after a dummy optional field is added | leader | | |
| W1.b | One corrupt row yields a 200 from `/v1/live` with an error entry | loop | | |
| W1.c | A v0.1 binary opening a v0.2 DB exits non-zero with a clear log line | leader | | |
| W1.d | A v2 run whose prompt text was deleted still verifies | leader | D7 | |
| W2.a | Mocked-verifier scenarios of A3-1/2/3/4 fail closed under the v2 category rules; the same evidence differs under @1 and @2 | leader | | |
| W2.b | A verification job whose claim was invalidated and re-made does not verify | loop | | `audit_a2_2_*` |
| W2.c | A backdated claim gets 400 | loop | | |
| W3.a | start@0, start@60 in one session gives agent time well under 60 min | loop | | `audit_a2_3_*` |
| W3.b | start@0, start@60, end@30, end@80 gives 50 min | loop | | `audit_a2_4_*` |
| W3.c | A paused run with agent work gives non-zero `agent_ms` | loop | | `audit_a6_1_agent_turn_while_paused_*` |
| W4.a | Two daemons started concurrently 50 times give 0 token mismatches; a mismatched data dir spools and `doctor` says why | leader | D3 | |
| W4.b | An upgrade with an SSE client open leaves exactly one daemon | leader | | |
| W4.c | `GET /v1/live?token=<full>` returns 401 | loop | | |
| W5.a | Adapter tool schemas come from `definitions()`, with a drift test; opencode or pi reach `evidence_verified` on tests-green | leader | | |
| W5.b | A repeated propose during an outage gives no duplicate milestones | loop | | |
| W6.a | An agent-token claim on `tax-compliance` does not count until the user confirms | leader | D6 | |
| W6.b | A cloned repo with `project.json` records nothing until `speedrun init --trust` | leader | | |
| W7.a | The PB shown comes from the same project | loop | | |
| W7.b | Upgrading the binary does not change a project's category version; the snapshot has a `protocol` field | leader | | |
| W8 | Reproducible packaging, no publish on a failing test, release attestation | leader | D1 | |
| W9.a | A log edited and re-hashed fails `speedrun verify` with a signature error | | | holds: `rewriting_and_rehashing_an_event_fails_the_signature_check` |
| W9.b | A frozen v1 fixture log verifies as unsigned | leader | | (constructed log: `unsigned_v1_logs_are_reported_not_failed`) |
| W9.c | Two device keys append to one run; key rotation keeps earlier events valid | leader | C1 | |
| W9.d | `speedrun export --tier 2` yields a re-derivable commitment and no prompt text | leader | | |
| P0.a | A run verifies with only public keys | leader | C1 | |
| P0.b | The daemon authenticates clients by socket peer UID | leader | D3 | |

## P1, a public record

| Item | Condition | Who | Decision | Test |
|---|---|---|---|---|
| P1.a | A locally forged `evidence_verified` is rejected by the server | leader | C1, C4 | |
| P1.b | A published run is reproducible from its bundle alone | leader | C4 | |
| P1.c | An inclusion proof verifies in a standalone test | loop | C4 | |
| P1.d | Deleting the local DB after publish loses nothing public | leader | C3, C4 | |

Decisions are made when the first item they block is next. D2 (ranking clock) is decided with
M5.b.
