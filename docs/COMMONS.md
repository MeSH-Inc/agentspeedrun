# agentspeedrun as a commons

**Status:** proposal, not built. P0 is `V0.2.md`; this document is v0.3 and later.

## 1. Premise

Today's product is a private stopwatch: one person, one machine, a log nobody
else can see. That is a good v0.1, but it leaves the interesting thing unused —
a timed run is only worth something when someone else can compare it to their
own.

Reframe it as a **commons**: a shared, public record of how fast and how
ambitiously people build real software with agents. What is competed for is not
data and not money; it is **credibility and attention**. What is contributed to
the commons is a **published run**: a signed, verifiable, comparable bundle of
timings, evidence and (optionally) prompts.

The trade is explicit and one-directional:

> You show more of your work than you have to, and in exchange your number is
> one that other people can believe.

Speed without evidence is a tweet. Evidence without speed is a changelog. The
commons is the place where the two are worth the same thing.

### What is shared, and what is not

| Thing | Owner | Leaves the machine? |
|---|---|---|
| The full event log, prompts at full fidelity | the author | never, by default |
| Timing, milestones, evidence references | the author | only when published |
| Prompt text | the author | only at an explicit disclosure tier, per run |
| Device signing key | the author | public key only |
| Verdicts ("this counted") | the commons | recomputed server-side, never taken from the client |
| Rulesets, boards, disputes | the commons | public, forkable |

## 2. Principles

1. **The record stays local; the disclosure is public.** These are different
   artifacts and must have different lifetimes, permissions and schemas.
2. **Nothing published can be silently edited.** Corrections are new signed
   objects that supersede old ones; the old ones stay readable.
3. **The author's own verdict is a preview, never the result.** Every published
   claim is re-verified by a party that is not the claimant.
4. **Transparency is chosen per run, not per install.** A person can publish one
   run at full transcript and the next at timings only.
5. **Plural boards, not one leaderboard.** Every ranking names a ruleset, a
   filter and a sort. No universal number that everyone fights over.
6. **Badges over scores.** Composite scores get gamed the week they appear;
   named, checkable properties do not.
7. **No tokens, no chain-of-blocks.** A transparency log is a Merkle tree with
   witnesses. Consensus is a cost this project does not need to pay.
8. **A commons needs a steward.** Rules, hosting, disputes and abuse need a
   named owner, published policy and an appeal path. Otherwise it is a forum
   with a database.

## 3. Identity and ownership

The v0.1 daemon proves "a process could read a file". The commons needs three
distinct identities, and they must not be conflated:

| Identity | Is | Proves | Changes |
|---|---|---|---|
| **Person** | GitHub account, optionally linked to a domain and a Stripe account | control of a repo, a domain, a payment account | rarely |
| **Device** | ed25519 keypair generated on first run | "this machine produced these events, continuously" | on new hardware |
| **Project** | `project_id` + remote URL | a namespace, not an owner | never; new project = new id |

The unit of ownership is the **person**. A device is a subordinate, revocable
key. This is the D5 decision from `V0.2.md:199`, and it should be made now: PB
pools, multi-device runs and revocation all depend on it.

### Linking

```
device key  ──signs──▶  claim: {run_id, device_pubkey, person_id, issued_at}
person      ──signs──▶  same claim via GitHub OAuth (server verifies the token's login)
```

Both signatures are required, so neither the server nor the device can invent a
link. Links, revocations and key rotations are append-only objects with
`supersedes` fields — never edits. A revoked key keeps its history (it may have
been honest then) but cannot sign new publications.

### What a person must be able to prove to claim ownership

| Claim | Proof | Cost to fake |
|---|---|---|
| This repo is mine | GitHub token with `repo` on that owner, or a signed commit in the remote | account compromise |
| This site is mine | DNS TXT `_speedrun.<registrable-domain>` or `/.well-known/speedrun.json` | domain control |
| This payment is my customer's | Stripe account id of the key equals the account owning the charge | Stripe account access |
| This run is mine | device signature + person/device link | key theft only |

Anything that cannot meet one of these stays `self_declared` or
`agent_claimed`. That is fine; the commons displays the level and the reader
decides.

## 4. The record: what changes in the log

Keep the v0.1 shape (gapless, hash-chained, append-only) and add signatures.
The invariants become:

```
canonical_json(payload) → hash  ── extends ──▶ prev_hash
hash                    → sig   ── signed by ─▶ device key
```

`sig` and `hash` are both excluded from the canonical body. The hash covers
content; the signature covers the hash; the chain covers order. A verifier
needs only the public key.

`RunStarted` gains additive fields (no schema break beyond v0.2's bump):

```rust
RunStarted {
    project_id: String,
    project_root: Option<String>,   // never published
    category_id: String,
    category_version: u32,
    // new
    device_pubkey: String,          // base32 ed25519
    start_head: Option<String>,     // HEAD sha at first prompt
    start_head_committed_at: Option<DateTime<Utc>>,
    ruleset_digest: String,         // sha256 of the resolved category file
    disclosure_commitment: String,  // sha256 of the per-run disclosure key
    chained_to: Option<String>,     // prior chain head, for resumed runs
}
```

Per-`(run, device)` chains: a run may have several chains, each signed by one
device and each starting with its own `RunStarted`. Projections merge them by
`occurred_at`. Two machines contributing to one run never rewrite each other.

## 5. Disclosure tiers and commitments

A published run carries exactly one **disclosure tier**. Tiers are per run, and
the publisher chooses. The server stores what it is given and nothing else.

| Tier | Public content | Typical use |
|---|---|---|
| **T0** | nothing | private runs; the default |
| **T1 stats** | clocks, category, milestone states, evidence URLs, repo URL | "I did it in 6h12m" |
| **T2 trace** | T1 + prompt count, per-prompt commitments, agent/model mix, tool counts | "here is the shape of the work" |
| **T3 excerpts** | T2 + selected prompts, author-curated, redacted | the interesting one |
| **T4 transcript** | T3 + every prompt after `redact()`, full turn timeline | "show your work" |

### Commit-then-reveal for prompts

Publishing prompt *text* is irreversible and often unwise. Publishing a
commitment is cheap and almost as convincing.

```
commitment_i = HMAC-SHA256(disclosure_key, prompt_text_i)
bundle       = { commitment_i, len_i, redacted_i, ... }
```

`disclosure_key` is generated per run, never leaves the machine, and its
sha256 is in `RunStarted`. A later **reveal** object publishes the key and the
text, letting anyone check that the text predates the commitment. So T2 gives
"the same 73 prompts, in this order, of these lengths" with cryptographic
proof that the transcript was not rewritten after the fact — without publishing
a single word.

This is strictly better than the V0.2 `HMAC(device secret, content)` digest for
commons purposes: the device secret can never be revealed, so its digests are
permanently opaque.

**Redaction remains best-effort.** The v0.1 audit (A1-2, A1-3) shows it misses
JSON-quoted keys, Bearer headers, credentialed URLs and private-key paste. A
review pass before T3/T4 publication is a product requirement, not a nicety:
`speedrun publish --tier 4` should print the diff between what will be sent and
what is stored, and refuse silently-empty redactions that look wrong.

## 6. Verification: preview and judgment

The single most important architectural split:

```
local daemon   →  verification preview   ("would this count?")
commons server →  judgment               ("this counts, under rules X, observed Y")
```

Both call the **same deterministic verifier library**. The difference is that
the server's inputs are external and its output is signed and anchored, so a
third party can re-run it years later.

A judgment records, per claim:

```rust
Judgment {
    bundle_digest: String,
    claim_ref: (run_id, milestone_id, chain_id, seq),
    rule: String,                 // "deployed@2"
    verdict: Trust,               // evidence_verified | rejected(reason)
    observed: Value,              // {status, final_url, fetched_at, digest, ...}
    verifier_key: String,
    issued_at: DateTime<Utc>,
}
```

`observed` holds the *facts* the verifier saw, each with a digest of the bytes
it saw, so a re-runner can tell "you were lied to" from "you used a different
rule".

### External clocks: the anti-inflation primitive

Local wall time is a number the author can trivially shrink by starting the
clock late. The commons fixes this without a trusted client by bracketing each
run against timestamps the author does not control:

| Source | Direction | Trust |
|---|---|---|
| Log ingest time of the first bundle | upper bound on start | steward |
| GitHub Actions `created_at` / `completed_at` | bounds inside the run | GitHub |
| GitHub push event received time | bounds on commit time | GitHub |
| Stripe charge `created` / `paid_at` | bounds on revenue | Stripe |
| First-seen crawl of the deployment URL | upper bound on "it was live by then" | steward |
| Git commit times | weak, author-controlled | unusable alone |

Publish `elapsed` together with `bracketed: [lower, upper]`. A run whose claimed
elapsed is below its bracket is rejected, not debated. This turns "trust me, 4
hours" into "at most 4h40m, by GitHub's clock and ours". It also gives honest
fast runs something to point at.

## 7. Publication protocol

```
speedrun publish --tier 3 --board rust/hundred-percent
```

1. **Build** — produce a bundle: chains (T0 truncation rules applied), evidence
   manifest, disclosure projection, commitments, device signature over the
   bundle digest, optional person/device link, optional disclosure key.
2. **Upload** — `POST /v1/bundles`, idempotent on `bundle_digest`. The server
   stores the bundle verbatim; nothing is normalized in place.
3. **Re-verify** — the server resolves each evidence reference independently
   (its own HTTP client, its own GitHub token, its own Stripe key), records
   `observed`, and issues a signed `Judgment`.
4. **Anchor** — the bundle digest and the judgment digest are appended to the
   transparency log (§8) and get inclusion proofs.
5. **Index** — the board lists the run at the tier the author chose.

Nothing in step 3 uses the client's verdicts, and step 3 failing is a normal
outcome, not an error. A run can be published with every claim `self_declared`;
it simply sorts below verified ones and carries no badges.

## 8. The transparency log

An append-only Merkle tree, Certificate-Transparency style:

- leaves are `sha256(bundle_digest ‖ judgment_digest ‖ board)`.
- the steward publishes signed tree heads periodically (at least daily, and in
  the repo for runs that matter).
- anyone can request an inclusion proof for their run; the web UI shows it.
- **witnesses**: independent parties that fetch and re-sign tree heads. A
  steward that tries to rewrite the log is caught by any witness that kept an
  older head. Start with one witness (the org's CI), document how to add more.

No consensus, no mining, no tokens. If external anchoring is ever wanted, it is
a `Anchor` object that points at a public timestamping service — a plug-in, not
a dependency.

## 9. Boards, rulesets and governance

```
board = (ruleset, filter, sort)
```

- **ruleset** — `category@version` with a digest, copied from the repo's
  `categories/` files. Changing a rule means a new version; old runs keep the
  rules they ran under. This is already true in v0.1 and is the commons'
  constitution.
- **filter** — language, host (Claude Code / opencode / pi / VS Code), model
  family, solo vs multi-device, project size band.
- **sort** — elapsed time, bracket width, badge count, or "newest".

Rules change by pull request to a public `rules` repo, and every rule change
must ship **acceptance scenarios**: mock evidence sets that must produce the
intended verdict before and after. Reviewers ask "what does this nerf?" and the
diff has to answer it.

**Ambition is not a number.** The countable part (category, number of verified
milestones, breadth of evidence kinds) is shown as-is. The rest is a short
author-written `story` field with links, and a `reviewed_by` list of people who
read the run. Pretending ambition is a scalar invites the worst kind of gaming.

## 10. Corrections, disputes, appeals

A commons dies the first time someone is accused of cheating and nobody can
resolve it.

- **Correction** — a new signed object `supersedes: <judgment_digest>` with the
  fixed claim. Boards show the latest; the old judgment stays readable.
  Honest mistakes cost a correction, not a deletion.
- **Dispute** — anyone can file one against a published run with a reason and
  evidence. A dispute is a log entry too. The board shows `disputed`, and the
  run keeps its numbers but loses its badges until resolved.
- **Resolution** — a steward signs an outcome with a rationale. Outcomes are
  published in a monthly ledger so patterns (prolific disputers, slow
  resolutions) are visible.
- **Appeal** — one per dispute, to a second steward. Two names, both public.

Runs cannot be unpublished by their author once anchored (that would break the
log). They can be *superseded* and the index can hide them from default views.
Opting out at publication time is the only real delete, which is why tier
selection is a deliberate step.

## 11. Incentives and what gets gamed

What the commons rewards, and how each reward resists capture:

| Rewarded | Mechanism | Failure mode | Mitigation |
|---|---|---|---|
| Speed | elapsed, bounded by external clocks | clock manipulation | brackets (§6) |
| Completion | verified milestones | evidence reaches | control proofs (§3) |
| Ambition | category ladder + verified breadth + story | narrative inflation | reviewer list, no score |
| Transparency | badges: `transcript-open`, `fully-verified`, `bracketed`, `device-linked`, `domain-verified` | badge farming | badges are properties, not points |
| Consistency | multiple runs on one identity | sockpuppets | person/device link, account age, witness visibility |

Badges, not a score, is a deliberate choice: a single number becomes the target
and stops measuring anything. Five badges let a reader weigh "fast but opaque"
against "slower but fully open", which is the actual conversation the commons
wants to have.

The one thing never rewarded: spending more money on tokens. Agent cost is
recorded (when the host reports it) and displayed, never ranked. Otherwise the
commons becomes a leaderboard for who can afford the most parallel agents.

## 12. Threat model

| Adversary | Goal | Defense |
|---|---|---|
| Curious reader | see prompts the author didn't publish | tiers; server stores only the projection |
| Cheating author | a faster run than happened | external clocks, server re-verification, commitments |
| Cheating author | a bigger project than theirs | domain/repo/Stripe control proofs |
| Malicious agent | forge claims via MCP | daemon already refuses `evidence_verified`; server recomputes |
| Malicious agent | prompt-inject the publish step | publish is human-invoked, shows the diff, requires a tier flag |
| Steward | rewrite or reorder history | Merkle log + witnesses + published tree heads |
| Steward | favor a friend | published judgments, dispute ledger, appeals |
| Sockpuppet | fake customer revenue | Stripe account id match + linked-account exclusion |
| Network attacker | intercept verification | fixed UA, TLS-only, public-address checks (v0.1) |

Explicit non-defense: prompts can contain third-party material; redaction is
best-effort; a determined author with a second machine and patience can still
manufacture a plausible run. The commons raises the cost from "type a number"
to "run a fake project for a week and keep the timestamps consistent" — which is
roughly the bar a hobby leaderboard should set.

## 13. Components

```
speedrund            local daemon + device key + preview verification   (exists)
speedrun publish     bundle builder, tier projection, diff preview      (new)
speedrun-verify      deterministic verifier library, shared by both     (refactor)
commons (server)     ingest, verify workers, Merkle log, boards,
                     accounts/links, disputes                          (new)
speedrun-web         read-only board + run pages + inclusion proofs     (new)
```

The verifier library being shared is the load-bearing design choice. If local
and server verification drift, every published number becomes arguable.

### Server data model (sketch)

| Table | Holds |
|---|---|
| `people` | GitHub id/login, display name, links |
| `devices` | pubkey, first seen, revoked_at |
| `links` | person↔device, signatures, supersedes |
| `runs` | run_id, person, project, ruleset, tier, public summary |
| `bundles` | digest, bytes (or object store key), tier, received_at |
| `events` | parsed from bundles, immutable |
| `observations` | fetch facts + response digests per claim |
| `judgments` | verdicts, rule, observed, verifier signature |
| `log_leaves` | Merkle leaf index + digests |
| `disputes` | target, reason, state, resolution |

Storage is cheap because everything is text and small: a T2 bundle for a 6-hour
run is a few hundred KB. The expensive part is the verifier workers and the
crawler, not the data.

## 14. Phases

### P0 — foundations (this is `V0.2.md`, plus signing)

Deliverables: port-file transport with token HMAC client auth (D3); device ed25519 keypair; `device_id` =
fingerprint; per-`(run, device)` chains; `sig` on every event; disclosure key +
commitment in `RunStarted`; ruleset digest; `observed` facts in
`MilestoneVerified`; per-run disclosure tier field locally.

Acceptance: a two-device run folds correctly; a run verifies with only public
keys; a tampered event fails signature check; `V0.2` acceptance tests still pass.

### P1 — a public record, no leaderboard

Deliverables: `speedrun publish --tier {0,1,2}`; bundle format v1 with a JSON
Schema in `schema/`; server ingest + re-verify + signed judgment; Merkle log
with signed tree heads and inclusion proofs; GitHub OAuth for persons; a run
page that shows clocks, bracket, badges, and judgments.

Acceptance: a locally forged `evidence_verified` is rejected by the server; a
published run is reproducible from its bundle alone; inclusion proof verifies in
a standalone test; deleting the local DB after publish loses nothing public.

### P2 — control proofs and brackets

Deliverables: domain TXT / `.well-known` verification; repo ownership check via
token; Stripe account-id match; external-clock brackets and the deploy crawler;
rejection of runs below their bracket.

Acceptance: a run with a URL not controlled by the claimant cannot reach
`evidence_verified`; a backdated run is rejected; a run whose Stripe payment is
from the claimant's own account id is rejected regardless of email.

### P3 — disclosure, boards, disputes

Deliverables: T3/T4 with the publish diff and redaction review; reveal objects;
boards as `(ruleset, filter, sort)`; badges; disputes, corrections, appeals;
monthly steward ledger; second witness.

Acceptance: a reveal proves a T2 commitment; a superseding correction changes
the board without breaking the inclusion proof; a disputed run loses badges and
regains them only on resolution.

### P4 — federation (not now)

Multiple independent logs with cross-log inclusion, mirroring, and an optional
external anchor. Explicitly deferred until there is more than one steward.

## 15. Non-goals

- A single global all-time leaderboard. It will be won by whoever optimizes the
  metric hardest, and the metric is not the point.
- An "ambition score". Narrative plus countable breadth, or nothing.
- Publishing prompts by default, or ever without a per-run human choice.
- Tokens, coins, NFTs, "proof of build".
- Deleting anchored runs.
- Ranking by token spend.
- Windows-native support (unchanged from v0.1: use WSL).

## 16. Owner decisions

C1, C3 and C4 were made by the owner on 2026-11-01. The rest block only P2 and later,
which `END_STATE.md` puts out of scope, and are deferred until then.

| # | Decision | Options | Blocks | Choice |
|---|---|---|---|---|
| **C1** | Identity unit | person vs device | P0 chains, P1 accounts, PB pools | person; devices are revocable keys |
| **C2** | Steward | MeSH-Inc as steward vs neutral foundation vs single-person hobby | P2 disputes, P3 governance | deferred |
| **C3** | Default publication | T1 on `speedrun finish` (prompted) vs always manual | P1 uploads | always manual: nothing leaves the machine until `speedrun publish` |
| **C4** | Server language | reuse Rust (`axum` + `sqlx`) vs separate stack | P1 | Rust, `axum` + `sqlx`, a crate in this workspace sharing the verifier library |
| **C5** | Moderation capacity | who answers disputes, expected SLA | P3 | deferred |
| **C6** | Rules repo | part of this repo vs separate `agentspeedrun/rules` | P3 | deferred |
| **C7** | Funding | sponsorship vs hosting the steward pays for | P3+ | deferred |

## 17. Relationship to `V0.2.md`

`V0.2.md` is P0 and should be shipped first and unchanged in scope. This
document only adds the two things v0.2 deliberately left out: a device key
(P0) and anything that leaves the machine (P1+). Nothing here requires the
v0.2 acceptance tests to change.

The one place to be careful: v0.2's `HMAC(device secret, content)` digest is
not revealable. P0 should introduce a separate per-run **disclosure key** so
that P3's commit-then-reveal works without a second schema bump.

## 18. Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| Commons with no participants | medium | P1 ships before any ranking; a record is useful alone |
| Metric Goodharting on speed | high | plural boards, badges, ambition axis, brackets |
| Dispute load exceeds capacity | medium | one steward + published SLA is honest; cap by requiring evidence to file |
| Legally sensitive prompt content | medium | tier default T2, redaction review, takedown path with supersede |
| Verifier drift local vs server | medium | shared library; a CI test that runs both on a fixture bundle |
| Server becomes the single point of trust | high | log + witnesses + published verifier; anyone can re-run a bundle |

If only one sentence survives review: **the daemon stays a personal instrument,
and the commons is a publishing pipeline around it that recomputes every claim
and cannot rewrite its own past.**

## 19. P1 contract (Wave 4)

Fixed by the leader for the Wave 4 tests. Scope is the record core: bundle, publish, ingest,
judgment and log. GitHub OAuth, the run page and deployment wait for Wave 5. Decisions taken here,
each with its reason:

- *Tiers.* Tier 0 anchors the run's chain heads and publishes no event. Tiers 1 and 2 publish the
  whole signed log, because a signed chain cannot drop events and still verify; the log carries
  prompts only as digests under the device's prompt key, which never leaves the machine. Tier 2 adds
  one commitment per prompt under the run's disclosure key, revealable in P3. How much of a tier 1
  log a page shows is the run page's concern (Wave 5).
- *Finished runs only.* A bundle is a final record, so only a completed or abandoned run is
  bundled; corrections are later publications, never edits.
- *No local paths.* A run's log is published verbatim, so it must not hold the project root. New
  runs write `RunStarted` without `project_root`; the daemon keeps each run's root in its own
  database, outside the log, for git checks and the live display. A run whose stored log holds a
  `project_root` cannot be bundled.
- *Judgment.* The commons runs `speedrun-verify` with no project root and no secrets of the
  author's. So in P1 it can verify URL evidence and nothing that needs the repository or the
  author's Stripe key; those claims keep their claimed trust capped at `agent_claimed`, and P2's
  control proofs widen this. Judgments carry verdicts and problems; the `observed` facts of §6
  arrive with P2's crawler.
- *Storage.* `speedrun-commons` is a crate in this workspace: axum and sqlx on SQLite (decision
  C4; the owner chose SQLite on 2026-11-02, so no hosting choice is needed yet). speedrund pins
  rusqlite 0.39, which shares `libsqlite3-sys` with sqlx 0.9.

**Bundle (`speedrun_core::bundle`, format 1, schema `schema/bundle.schema.json`).** `events` are
the run's stored bodies, verbatim, ordered by `device_id` then `seq`. `digest` is the canonical
digest without `signature`; `signature` is `sign::sign_hash(device key, digest)`; the publisher key
must sign one of the run's chains (its `RunStarted`/`DeviceJoined` key or a `DeviceRotated` new
key). Tier 2's `prompts` are `speedrun export`'s commitments, one per `prompt_submitted` event.

**Daemon.** `GET /v1/runs/{run_id}/bundle?tier=<0|1|2>`, full token only (agent and display tokens
401), returns the signed `Bundle` from the current device key. 409 when the run is not finished or
its log holds a `project_root`, and at tier 2 when its `RunStarted` names no disclosure commitment;
400 for another tier.

**CLI.** `speedrun publish --tier <0|1|2> --run <run_id> [--to <url>]` fetches the bundle from the
daemon and posts it to the commons at `--to`, else `SPEEDRUN_COMMONS_URL`; with neither it exits 1
and sends nothing. Publishing is always a command the user runs (C3). On success it prints
`published <bundle_digest>`, then `leaf <index> of <tree_size>`, then one line per milestone
judgment, `<milestone_id>: <granted or none>` followed by ` (<problems joined by "; ">)` when
there are problems.

**Commons (`speedrun-commons`).** `SPEEDRUN_COMMONS_HOME` is its data directory (`commons.key`,
`commons.sqlite`); `SPEEDRUN_COMMONS_ADDR` its address, 127.0.0.1:7478 by default. Types are in
`speedrun_core::commons`. Every signature is `sign::sign_hash(commons key, digest)` over
`judgment_digest` or `tree_head_digest`, checkable with `/v1/health`'s `commons_pubkey`.

- `POST /v1/bundles` takes a `Bundle`. It is refused with 400 and nothing stored unless all of
  these hold: `format` is 1 and `tier` at most 2; the signature verifies over `digest`; at tier 0,
  `events` and `prompts` are empty, `chain_heads` is not, no two heads name the same `device_id`,
  and each head's `hash` is 64 lowercase hex digits; at tiers 1 and 2, `chain_heads` is
  empty, every event names the bundle's `run_id`, `hash::verify_stored_chain` passes,
  `sign::verify_event_signatures` passes, the publisher key signs one of the run's chains, the run
  folds and is finished, no `RunStarted` holds a `project_root`, and `prompts` is empty at tier 1
  and at tier 2 names each `prompt_submitted` event exactly once. A valid bundle is stored with its
  body exactly as received, judged, signed, and appended as the next leaf; the answer is a
  `PublishReceipt` with the new signed tree head and the leaf's inclusion proof against it. A
  bundle whose digest is already stored is answered with its existing judgment and leaf, a current
  tree head and a proof against it, and adds no leaf. A tier 0 bundle has no events to fold, so
  its judgment has no milestones and its published run has no `view`. The commons parses, folds
  and judges a bundle's events in run order (`event::run_order`: each device's chain by `seq`,
  merged by `recorded_at`, a tie going to the smaller device id), and judges with no secrets, so
  only what the public network shows can be granted. One publication is appended at a time.
- *Judging* (`judge::judge`). For each milestone of the folded run, in order: `claimed` is its
  trust. If its current claim (the last `milestone_reached` since the last invalidation) carries
  checkable evidence, the commons builds a `speedrun_verify::Job` from that claim (its event id,
  key, evidence and `occurred_at`; the run's start; no project root; as `deployed_url` the URL of
  the run's `deployed` claim when the commons verified it) and runs the verifier: if it passes,
  `granted` is `evidence_verified` with no problems; otherwise `granted` is `claimed` capped at
  `agent_claimed` and `problems` are the verifier's. Without checkable evidence, `granted` is
  `claimed` capped at `agent_claimed`. An unclaimed milestone has neither.
- *Log* (`speedrun_core::merkle`, RFC 9162 §2.1). A leaf is `leaf_hash(bundle_digest,
  judgment_digest)`, the hash of the text `<bundle_digest>:<judgment_digest>` with the 0x00 leaf
  prefix. Leaf indices are 0, 1, 2, … in arrival order. Every append issues a signed `TreeHead`.
- `GET /v1/log/head` is the latest tree head (for an empty log, size 0 and the root of the empty
  tree, signed). `GET /v1/log/proof/{leaf_index}?tree_size=<n>` is the `InclusionProof` against the
  head of size `n` (default the current size): 404 when the leaf is not in that tree, 400 when `n`
  exceeds the log.
- `GET /v1/bundles/{digest}` and `GET /v1/runs/{run_id}` return the `PublishedRun` of that bundle,
  or of the run's latest publication: the bundle as stored, its judgment, its leaf index, and at
  tiers 1 and 2 the view `RunView::fold(events, the latest occurred_at among them)`. 404 when
  unknown.
- `GET /v1/health` is `CommonsHealth`.


## 20. P1 contract (Wave 5)

Fixed by the leader for the Wave 5 tests. Scope: GitHub identity for persons, the public pages,
and running the commons on a host. Decisions taken here, each with its reason:

- *Linking by device flow.* `speedrun link` links the daemon's device key to a GitHub account. The
  commons runs GitHub's OAuth device flow on the device's behalf and reads `/user` once with the
  token, so the token never reaches the user's machine and never reaches the commons' disk. The
  commons keeps no sessions or cookies; its pages are read-only. (Owner, 2026-11-02.)
- *Both signatures.* The device signs a `LinkClaim` naming the person GitHub reported, and the
  commons countersigns the same digest. Neither can invent a link alone, and `commons_pubkey` in
  the claim stops it being replayed at another commons (§3).
- *Not in the log.* Links are stored rows, append-only, not Merkle leaves: the leaf format of §19
  stays `bundle:judgment`. Revocation and key rotation of links wait for P2.
- *Pages.* Server-rendered HTML from the commons crate with `maud`, which escapes every
  interpolated value by construction; no JavaScript. A chronological list of recent
  publications, a run page and a person page, and no ranking (§9 waits for P3). Badges and
  external-clock brackets arrive with P2 and P3. (Owner, 2026-11-02.)
- *Hosting.* Railway, one service built from the repository's `Dockerfile`, a volume at `/data`
  for the database and the commons key, at the host's generated address. (Owner, 2026-11-02.)

**Configuration.** `SPEEDRUN_GITHUB_CLIENT_ID` names the GitHub OAuth app (device flow enabled);
without it `POST /v1/links` answers 503 and nothing else changes. `SPEEDRUN_GITHUB_URL` (default
`https://github.com`) is the base of `/login/device/code` and `/login/oauth/access_token`;
`SPEEDRUN_GITHUB_API` (default `https://api.github.com`) the base of `/user`. Every GitHub request
sends `Accept: application/json` and `User-Agent: speedrun-commons`, and no email address anywhere.
When `SPEEDRUN_COMMONS_ADDR` is unset and `PORT` is set, the commons listens on `0.0.0.0:$PORT`
(a `PORT` that is not a port number is an error); with neither, on `127.0.0.1:7478`.
`SPEEDRUN_COMMONS_PUBLIC_URL` is the address the run page's `curl` commands name; without it they
name the placeholder `$COMMONS`, never the request's `Host` header, which a cache need not key on.

**Links (commons).** All bodies are JSON; types are in `speedrun_core::commons`.
- `POST /v1/links` takes `LinkStart`. 400 unless `device_pubkey` is 64 lowercase hex. The commons
  posts `client_id` and an empty `scope` to `<github>/login/device/code` and answers 200 with
  `LinkStarted`: a fresh `link_id` (a ULID), GitHub's `user_code` and `verification_uri`, its
  `interval` as `interval_seconds`, and `expires_at` = now + its `expires_in`. A GitHub failure is
  502. The pending link (device key, device code, expiry) is stored.
- `POST /v1/links/{link_id}/poll` (body `{}`). 404 for an unknown link; 410 once `expires_at`
  has passed, authorized or not: the device has GitHub's whole expiry window to confirm. If the
  link is already authorized, 200 with the same `LinkState::Authorized`. Otherwise the commons makes one
  request to `<github>/login/oauth/access_token` with `client_id`, the device code and
  `grant_type=urn:ietf:params:oauth:grant-type:device_code`: `authorization_pending` or
  `slow_down` is 202 `LinkState::Pending`; `expired_token` or `access_denied` is 410 and the link
  can never complete; a token is used at once for `GET <api>/user` (`Authorization: Bearer`), the
  `id` and `login` become the link's person, the token is dropped, and the answer is 200
  `LinkState::Authorized`. A GitHub failure is 502 and changes nothing.
- `POST /v1/links/{link_id}/confirm` takes `LinkConfirm`. 404 for an unknown link; 409 until it is
  authorized; 410 once it expired or was refused. 400 unless `claim.format` is 1, `claim.link_id`
  is the path's, `claim.device_pubkey` is the one the link started with, `github_id` and `login`
  are the authorized person's, `commons_pubkey` is this commons' key, and `device_signature`
  verifies over `link_claim_digest(claim)` with `claim.device_pubkey`. Then the commons signs that
  digest and stores and answers 200 with the `Link`. Confirming again answers the stored `Link`.
- A login names at most one person: the one who most recently linked with it. When a link is
  stored, any other person holding that login (compared without regard to case) loses it and is
  shown by `github id <id>` from then on, keeping their links and publications.
- `GET /v1/people/{login}` is `PersonRecord`, `login` matched without regard to case against the
  current login of each person: every link (oldest first) and every publication whose bundle's
  `publisher_pubkey` is a linked device key (newest first). 404 when no link names that login.
- A person's publications are those signed by any device key linked to them. A publication is
  shown as that person's on every page.

**Daemon.** Both routes take the full token only (other tokens 401). `GET /v1/device` answers
`DeviceKey` with the daemon's current device key and its fingerprint. `POST /v1/link-claims` takes
a `LinkClaim` and answers `DeviceSignature`: `sign::sign_hash(device key, link_claim_digest(claim))`.
400 unless `format` is 1 and `device_pubkey` is the daemon's current device key. It signs nothing
else.

**CLI.** `speedrun link [--to <url>]` resolves the commons like `publish` (`--to`, else
`SPEEDRUN_COMMONS_URL`, else exit 1 sending nothing). Both commands refuse a commons address that
is not `https://`, unless it is plain `http://` to `localhost`, an IPv4 loopback address or
`[::1]`: the commons names the person a device links to, so no network path may answer for it. It reads the device key from the daemon
(`GET /v1/device`), posts `LinkStart`, prints
`open <verification_uri> and enter <user_code>`, then polls every `interval_seconds` until
authorized (exit 1 on 410 or once `expires_at` passes). It asks `GET /v1/health` on the commons for
`commons_pubkey`, builds the `LinkClaim` with `issued_at` now, has the daemon sign it, confirms, and
prints `linked <login> (github id <id>) to device <fingerprint>`.

**Pages.** `GET /`, `GET /runs/{run_id}` and `GET /people/{login}` answer `text/html;
charset=utf-8` with `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'` and a
viewport meta tag, and lay out in one column that fits 320 px (wide tables scroll inside their
own box). Unknown runs and people are 404 pages in the same shape.
- `/` lists the 50 newest publications, newest first: a link to `/runs/{run_id}`, the tier, the
  received time (UTC, RFC 3339 to the second), and the person's login linked to
  `/people/{login}`, `github id <id>` for a person who lost their login, or `unlinked device`
  with the publisher key's fingerprint. A tier 0 publication has no category, as it has no events.
- `/runs/{run_id}` shows the run's latest publication: run id, category `id@version`, the person
  as on `/`, tier and received time. At tiers 1 and 2: the clocks `wall`, `agent` and `human` from
  the published view, each as `H:MM:SS`; and one row per judged milestone with its title, required
  trust, claimed and granted trust (snake case, `none` when absent) and its problems. At tier 0:
  the anchored chain heads (device id, seq, hash) and no clocks or milestones. Then the record: the
  bundle digest linked to `/v1/bundles/{digest}`, the judgment's signature and the commons key,
  the leaf index, the latest tree head's size and root, the leaf's audit path against that head,
  and the two `curl` commands that fetch the bundle and the proof.
- `/people/{login}` shows the login and GitHub id, each linked device (fingerprint, linked time),
  and the person's publications as `/` lists them.

**Backup.** `speedrun-commons backup <file>` writes a consistent copy of the database at
`$SPEEDRUN_COMMONS_HOME` to `<file>` (SQLite `VACUUM INTO`) while a server may be running, prints
`backed up <n> leaves to <file>` with `n` counted in the copy, and exits 0; it refuses to overwrite an existing file
(exit 1). The commons key is not in the database: the operator copies `commons.key` once.

**Deployment (leader).** `Dockerfile` builds `speedrun-commons` and runs it with
`SPEEDRUN_COMMONS_HOME=/data`; `railway.json` sets the health check on `/v1/health` and one
replica; `docs/DEPLOY.md` is the runbook: service, volume, variables, a backup before every
deploy, the smoke test afterwards, and restore.
