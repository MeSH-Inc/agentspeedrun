# agentspeedrun end state

**Status:** accepted 2026-11-01. The end state is COMMONS **P1**: a published run that anyone
can believe without trusting the author's machine. It is reached in two stages: v0.2
([V0.2.md](V0.2.md), which is COMMONS P0) and P1 ([COMMONS.md](COMMONS.md) §14). P2 and later
are out of scope: most of what they need is governance (C2, C5, C7), which no code change
advances.

This document is an index. What each property means is defined in `V0.2.md` and
`COMMONS.md`; this file lists the properties as checkable conditions, says who can make each
one true, and records where it stands.

## Terms

- **Property**: one condition of the end state with its acceptance check. Its id is the
  source item plus a letter (`W1.b` is the second acceptance item of workstream W1).
- **Reachability**: who can make a property true.
  - `loop`: a bounded change judged by an objective test and the required checks, touching
    no protected file (manifests, `crates/*/tests/`, `categories/`, `schema/`, existing unit
    tests). Worked by automated proposers under the ratchet.
  - `loop after test`: becomes `loop` once a leader writes its objective test.
  - `leader`: needs design, a schema or category change, a new harness, edits to existing
    tests, or code outside the Rust workspace (adapters, release, server).
  - A property waiting on an owner decision names it in **Decision** and is not started
    until the decision is made.
- **Objective test**: a test that states one property not yet true, marked
  `#[ignore = "audit <finding>"]` or `#[ignore = "<property id>"]` and named after it
  (`audit_a6_1_...`, `w1_b_...`). It fails until the property holds. Run with
  `cargo test --all -- --ignored <prefix>`.
- **Guard test**: a test that passes today and keeps a fix from passing an objective test by
  removing the behaviour under test (`guard_...`).
- **Promotion**: once an objective test passes on `main`, its `#[ignore]` is removed and it
  becomes an ordinary test, so every later change must keep the property.
- **Status**: `holds` (a test on `main` shows it), `objective` (an objective test exists and
  fails), `open` (no test yet).

## Stage 1: v0.2 (COMMONS P0)

### Must-fix (V0.2 §2)

| Property | Condition | Reachability | Decision | Status |
|---|---|---|---|---|
| M1.a | `speedrun hook`/`ingest` exit 0 on any argument error | loop | | objective: `audit_a4_1_*` (4) |
| M1.b | The marketplace plugin source is pinned to release tags | leader | D1 | open |
| M2.a | Token creation follows the single-instance bind; the client spools on 401, 5xx, timeout | leader | | open |
| M3.a | Daemon logs reach stderr or a capped file; startup logs version and data dir | loop after test | | open |
| M4.a | SIGTERM with an SSE stream open ends the daemon within about 2 s | leader | | open |
| M5.a | Agent work while paused counts; a prompt or turn start resumes the run | loop | | objective: `audit_a6_1_*` (2) |
| M5.b | `RunPaused` with an open turn is rejected | loop | | objective: `audit_a6_1_pause_with_an_open_turn_is_rejected` |
| M5.c | v0.1 PBs whose value changes under M5 are labelled | leader | D4 | open |
| M6.a | `git_commit` evidence committed before run start does not verify | loop | | objective: `audit_a3_1_*` |
| M6.b | A `ci_run` whose head commit predates run start does not verify | loop after test | | open |
| M7.a | Redaction covers JSON keys, Bearer, credentialed URLs, `*_PASS`, camelCase keys, `sk_test_`/`whsec_`, JWTs, truncated private keys | loop | | objective: `audit_a1_2_*` (8) |
| M7.b | The client redacts before sending or spooling; `rejected.jsonl` is capped | loop after test | | open |
| M8.a | A hook records its own event first and flushes the spool within about 3 s | leader | | open |
| M9.a | A user's claim satisfies `agent_claimed` on a non-predefined milestone | loop | | objective: `audit_a6_5_*` |
| M9.b | `accept` can set `required_trust` | loop after test | | open |
| M10.a | ARCHITECTURE, README, store.rs, package.sh and install.sh claim only what the code does | leader | | open |

### Workstreams (V0.2 §3)

| Property | Condition | Reachability | Decision | Status |
|---|---|---|---|---|
| W1.a | `speedrun verify` passes on a frozen v0.1 fixture DB after a dummy optional field is added | leader | | open |
| W1.b | One corrupt row yields a 200 from `/v1/live` with an error entry | loop after test | | open |
| W1.c | A v0.1 binary opening a v0.2 DB exits non-zero with a clear log line | leader | | open |
| W1.d | A v2 run whose prompt text has been deleted still verifies | leader | | open |
| W2.a | Mocked-verifier scenarios of A3-1/2/3/4 fail closed under the v2 category rules | leader | | open |
| W2.b | A finished verification job whose claim was invalidated and re-made does not verify (A2-2) | loop | | objective: `audit_a2_2_*` |
| W2.c | The same evidence gives different outcomes under @1 and @2 rules | leader | | open |
| W2.d | A backdated claim gets 400 | loop after test | | open |
| W3.a | start@0, start@60 in one session gives agent time well under 60 min | loop | | objective: `audit_a2_3_*` |
| W3.b | start@0, start@60, end@30, end@80 gives 50 min | loop | | objective: `audit_a2_4_*` |
| W3.c | A paused run with agent work gives non-zero `agent_ms` | loop | | objective: `audit_a6_1_agent_turn_while_paused_resumes_the_run` |
| W4.a | Two daemons started concurrently 50 times give 0 token mismatches | leader | D3 | open |
| W4.b | A hook with a mismatched data dir spools, and `doctor` says why | leader | D3 | open |
| W4.c | An upgrade with an SSE client open leaves exactly one daemon | leader | | open |
| W4.d | `GET /v1/live?token=<full>` returns 401 | loop after test | | open |
| W5.a | Adapter tests fail when a field of `definitions()` is missing | leader | | open |
| W5.b | opencode or pi reach `evidence_verified` on tests-green with a mocked CI response | leader | | open |
| W5.c | A repeated propose during an outage gives no duplicate milestones | loop after test | | open |
| W6.a | An agent-token claim on `tax-compliance` does not count until the user confirms | leader | D6 | open |
| W6.b | A cloned repo with `project.json` records nothing until `speedrun init --trust` | leader | | open |
| W7.a | The PB shown comes from the same project | loop after test | | open |
| W7.b | Upgrading the binary does not change a project's category version | leader | | open |
| W7.c | The live snapshot schema has a `protocol` field | leader | | open |
| W8.a | Packaging the same binaries twice gives an identical sha256 | leader | D1 | open |
| W8.b | A tag on a commit with a failing unit test does not publish | leader | D1 | open |
| W8.c | `gh attestation verify` succeeds on the release tarball | leader | D1 | open |
| W9.a | A log edited and re-hashed fails `speedrun verify` with a signature error | | | holds: `rewriting_and_rehashing_an_event_fails_the_signature_check` |
| W9.b | A frozen v1 fixture log verifies, reported as unsigned | leader | | open (a constructed v1 log is covered by `unsigned_v1_logs_are_reported_not_failed`; the frozen fixture comes with W1.a) |
| W9.c | Two device keys append to one run and `speedrun status` folds both | leader | C1 | open |
| W9.d | `speedrun export --tier 2` yields a commitment a test re-derives, with no prompt text | leader | | open |
| W9.e | Rotating the device key invalidates neither earlier events nor the chain | leader | C1 | open |

### COMMONS P0 additions

| Property | Condition | Reachability | Decision | Status |
|---|---|---|---|---|
| P0.a | A run verifies with only public keys | leader | C1 | open |
| P0.b | The daemon authenticates clients by socket peer UID | leader | D3 | open |

## Stage 2: P1, a public record (COMMONS §14)

| Property | Condition | Reachability | Decision | Status |
|---|---|---|---|---|
| P1.a | A locally forged `evidence_verified` is rejected by the server | leader | C1, C4 | open |
| P1.b | A published run is reproducible from its bundle alone | leader | C4 | open |
| P1.c | An inclusion proof verifies in a standalone test | loop after test | C4 | open |
| P1.d | Deleting the local DB after publish loses nothing public | leader | C3, C4 | open |

## Decisions on the path

Each is asked when the first property it blocks is next, not before.

| Decision | Source | Blocks |
|---|---|---|
| D1 license and public repo | V0.2 §5 | M1.b, W8.a–c |
| D3 transport | V0.2 §5 | W4.a–b, P0.b |
| D4 v0.1 history under the new fold rules | V0.2 §5 | M5.c |
| D6 agent trust default | V0.2 §5 | W6.a |
| C1 identity unit (subsumes D5) | COMMONS §16 | W9.c, W9.e, P0.a, P1.a |
| C3 default publication | COMMONS §16 | P1.d |
| C4 server language | COMMONS §16 | P1.a–d |

D2 (ranking clock) and D7 (prompt-text key) change how results are presented or which key
is used, not whether a property above can be made true; they are asked with M5.c and W1.d.

## Order of work

1. `loop` properties, with the objective tests above. They need no decision.
2. `loop after test` properties: a leader writes the objective and guard tests, then they
   join the loop.
3. `leader` properties with no decision, starting with W1 (every later item depends on it).
4. Decision-gated properties, as each decision is made.

When a property's status changes, this table changes in the same commit.
