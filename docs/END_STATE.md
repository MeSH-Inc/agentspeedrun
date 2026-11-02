# agentspeedrun end state

The end state is COMMONS **P1**: a published run anyone can believe without trusting the
author's machine. It is reached through v0.2 ([V0.2.md](V0.2.md), which is COMMONS P0). P2
and later are out of scope: they are mostly governance (C2, C5, C7).

This is an index of the acceptance items in `V0.2.md` and `COMMONS.md` §14, which define
them. **Who**: `loop` means a bounded change a test can judge, which an automated proposer
can make once the test exists; `leader` means it needs design, a schema or category change,
or code outside the Rust workspace. **Test**: the test that shows the item holds, or an ignored
objective test that fails until it does. A named `audit_` test that is still ignored is the open objective of its item.

## v0.2 (COMMONS P0)

| Item | Condition | Who | Decision | Test |
|---|---|---|---|---|
| M1.a | `speedrun hook`/`ingest` exit 0 on any argument error | loop | | `audit_a4_1_*` |
| M1.b | The marketplace plugin source is pinned to release tags | leader | D1 | |
| M2 | Token creation follows the single-instance bind; the client spools on 401, 5xx, timeout | loop | D3 | `audit_m2_*` |
| M3 | Daemon logs reach stderr or a capped file | loop | | `audit_m3_*` |
| M4 | SIGTERM with an SSE stream open ends the daemon within about 2 s | loop | | `audit_m4_*` |
| M5.a | Agent work while paused counts; a prompt or turn start resumes the run; pausing with an open turn is rejected | loop | | `audit_a6_1_*` |
| M6.a | `git_commit` evidence from before run start does not verify | loop | | `audit_a3_1_*` |
| M6.b | A `ci_run` whose head commit predates run start does not verify | loop | | `audit_m6_b_*` |
| M7.a | Redaction covers the shapes listed in M7 | loop | | `audit_a1_2_*` |
| M7.b | The client redacts before sending or spooling; `rejected.jsonl` is capped | loop | | `audit_m7_b_*` |
| M7.c | Spool and `rejected.jsonl` write failures are counted and `doctor` shows them | loop | | `audit_m7_c_*` |
| M7.d | Trimming `rejected.jsonl` loses no line under concurrent refusals | leader | | |
| M8 | A hook records its own event first and flushes the spool within about 3 s | loop | | `audit_m8_*` |
| M8.b | A timeout on a sent request is "delivered-unknown", not Unreachable; claim replays are idempotent by event id | leader | | |
| M9.a | A user's claim satisfies `agent_claimed` on a non-predefined milestone | loop | | `audit_a6_5_*` |
| M9.b | `accept` can set `required_trust` | leader | | `audit_m9_b_*` |
| M10 | The docs claim only what the code does | leader | | |
| W1.a | `speedrun verify` passes on an event carrying an optional field this build does not know; hashing covers the stored body | loop | | `audit_w1_a_*` |
| W1.b | One corrupt row yields a 200 from `/v1/live` with an error entry | loop | | `audit_w1_b_*` |
| W1.c | A binary opening a DB with a newer schema than its own exits non-zero with a clear log line | loop | | `audit_w1_c_*` |
| W1.d | Prompt text is kept outside the chain, digested under the device's prompt key; a run whose prompt text was deleted still verifies | loop | D7 | `audit_w1_d_*` |
| W1.e | An unsigned event, or a run naming no device key, fails `speedrun verify` | loop | D4 | `audit_w1_e_*` |
| W1.f | Predefined milestone ids are namespaced `<category>@<version>/<key>`; a category key still names one on input | loop | D4 | `audit_w1_f_*` |
| W2.a | Mocked-verifier scenarios of A3-1/2/3/4 fail closed under the current category rules | loop | D4 | `audit_w2_a_*`, `audit_a3_1_*` |
| W2.b | A verification job whose claim was invalidated and re-made does not verify | loop | | `audit_a2_2_*` |
| W2.c | A backdated claim gets 400 | loop | | `audit_w2_c_*` |
| W2.d | A spool replay (marked `X-Speedrun-Replay: 1`, set by flush) older than the run's last event is accepted; the lower bound also applies to the prompt route | loop | | `audit_w2_d_*` |
| W3.a | start@0, start@60 in one session gives agent time well under 60 min | loop | | `audit_a2_3_*` |
| W3.b | start@0, start@60, end@30, end@80 gives 50 min | loop | | `audit_a2_4_*` |
| W3.c | A paused run with agent work gives non-zero `agent_ms` | loop | | `audit_a6_1_agent_turn_while_paused_*` |
| W4.a | Two daemons started concurrently 50 times give 0 token mismatches; a mismatched data dir spools and `doctor` says why | loop | D3 | `audit_m2_concurrent_*`, `audit_w4_a_*`, `audit_p0_b_client_*` |
| W4.d | `speedrun doctor` shows versions, the data-dir match, spool, rejected and write-failure counts | loop | | `audit_w4_doctor_*`, `audit_m7_c_*` |
| W4.b | An upgrade with an SSE client open leaves exactly one daemon | leader | | |
| W4.c | `GET /v1/live?token=<full>` returns 401 | loop | | `audit_w4_c_*` |
| W5.a | Adapter tool schemas come from `definitions()`, with a drift test; opencode or pi reach `evidence_verified` on tests-green | leader | | |
| W5.b | A repeated propose during an outage gives no duplicate milestones | loop | | `audit_w5_b_*` |
| W6.a | An agent-token claim on `tax-compliance` does not count until the user confirms | leader | D6 | |
| W6.b | A cloned repo with `project.json` records nothing until `speedrun init --trust`; the project search stops at the git toplevel and `$HOME` | loop | | `audit_w6_b_*` |
| W7.a | The PB shown comes from the same project | loop | | `audit_w7_a_*` |
| W7.b | Upgrading the binary does not change a project's category version; the snapshot has a `protocol` field | leader | | |
| W8 | Reproducible packaging, no publish on a failing test, release attestation | leader | D1 | |
| W9.a | A log edited and re-hashed fails `speedrun verify` with a signature error | | | holds: `rewriting_and_rehashing_an_event_fails_the_signature_check` |
| W9.c | Two device keys append to one run; key rotation keeps earlier events valid | loop | C1 | `audit_w9_c_*` |
| W9.d | `speedrun export --tier 2` yields a re-derivable commitment and no prompt text | loop | C3 | `audit_w9_d_*` |
| W9.e | `RunStarted` names the start HEAD, its committer time, and the digest of the rules it pinned | loop | | `audit_w9_e_*` |
| P0.a | A run verifies with only public keys | loop | C1 | `audit_w9_c_rotated_chain_*`, `audit_w9_c_two_device_*` |
| P0.b | Clients find the daemon through `$DATA_DIR/addr` and authenticate by HMAC over the data-dir token | loop | D3 | `audit_p0_b_*`, `audit_w4_a_*` |

## P1, a public record

| Item | Condition | Who | Decision | Test |
|---|---|---|---|---|
| P1.a | A locally forged `evidence_verified` is rejected by the server | leader | C1, C4 | |
| P1.b | A published run is reproducible from its bundle alone | leader | C4 | |
| P1.c | An inclusion proof verifies in a standalone test | loop | C4 | |
| P1.d | Deleting the local DB after publish loses nothing public | leader | C3, C4 | |

The owner decided D1–D7 and C1, C3 and C4 on 2026-11-01; the choices are in `V0.2.md` §5 and
`COMMONS.md` §16. D4 retires every v0.1 format, which removed the items for labelling v0.1 PBs
(M5.b) and accepting unsigned v1 logs (W9.b) and added W1.e.
