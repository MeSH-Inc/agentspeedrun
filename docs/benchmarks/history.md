# Long-history and live subscriber baseline

This is a baseline before projection caching, measured on 2026-11-02. Raw command output is in [history-2026-11-02.txt](history-2026-11-02.txt). The source revision was `9c79cf5`, plus the benchmark-only module in this change. The production projection and HTTP paths were unchanged.

Machine: Apple M4 Pro, 12 logical CPUs, 24 GiB RAM, macOS 26.7.2, arm64, Rust 1.99.1 (58cf8f9c3 2026-10-13). The release profile uses optimization and thin LTO. Measurements ran locally, serially, with four Tokio runtime threads; they are not a controlled lab or a claim about other machines. No process CPU or memory metrics were collected.

Reproduce each case from the repository root:

```sh
BENCH_EVENTS=10000 BENCH_SUBSCRIBERS=5 BENCH_ITERATIONS=30 \
  cargo test --release -p speedrund history_pressure -- --ignored --nocapture
```

Repeat for `BENCH_EVENTS=1000,10000,100000` and `BENCH_SUBSCRIBERS=1,5,20`. Each case creates a fresh temporary SQLite database and a single active project with one signed, gapless run, category milestones, and metadata-only prompts and paired turns spread across eight sessions. A single transaction uses the actual store signing and insertion path to seed the requested number of events without invoking the service's repeated projections. The benchmark verifies the chain before timing HTTP. Events have increasing occurrence timestamps; writes after seeding continue the prompt/start/end cycle.

`load_deserialize` reads the run from SQLite into events; `fold` starts after that read and measures only `RunView::fold` over those in-memory events. These 30 samples precede the HTTP portion, so they do not include subscriber contention. HTTP uses a real loopback Axum server and clients, with the personal-best cache warmed before samples. Each cycle performs one GET `/v1/live`, one POST `/v1/events`, then waits until every SSE subscriber has consumed and parsed a complete snapshot with the target cursor. HTTP append response and write-to-last-subscriber timings are independent. SSE clients decode HTTP transfer framing and validate both JSON and cursor. Each case measures 30 sequential cycles; reported p95 is nearest-rank. The cycle rate includes both the GET and the wait for all subscribers and is not a maximum write throughput measurement.

The closed-loop barrier intentionally avoids overlapping writes, so it cannot characterize queue growth, watch-channel coalescing, open-loop saturation, stalled readers, multiple active projects, cold personal-best lookup, or longer histories with different event distributions. GETs happen between writes after subscribers have caught up; their latency is not a concurrent GET stress result. Counts are initial history sizes and increase by one per sample. Signed fixture creation and chain verification are outside timed samples. Results include service mutex contention, SQLite reads, serialization, loopback HTTP, SSE transport and client JSON parsing; they do not isolate the CPU cost of any one component.

## Results

All latency values are milliseconds, shown as p50 / p95. Rate is completed closed-loop cycles per second.

| Events | Subscribers | Load + deserialize | Fold | GET live | Append response | Last subscriber | Cycles/s |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 1 | 0.83 / 0.98 | 0.14 / 0.17 | 1.35 / 1.48 | 2.63 / 2.97 | 3.82 / 4.25 | 191.71 |
| 1,000 | 5 | 0.83 / 0.95 | 0.14 / 0.17 | 1.31 / 1.38 | 2.49 / 2.62 | 7.83 / 8.16 | 108.89 |
| 1,000 | 20 | 0.83 / 0.98 | 0.14 / 0.18 | 1.42 / 1.65 | 2.73 / 3.24 | 24.97 / 26.63 | 37.39 |
| 10,000 | 1 | 10.35 / 11.20 | 2.30 / 2.44 | 13.71 / 14.97 | 26.57 / 29.71 | 39.73 / 43.58 | 18.23 |
| 10,000 | 5 | 9.69 / 10.20 | 2.17 / 2.53 | 12.97 / 13.63 | 25.60 / 26.57 | 88.93 / 91.28 | 9.79 |
| 10,000 | 20 | 9.70 / 10.11 | 2.14 / 2.48 | 13.41 / 15.75 | 26.44 / 27.41 | 289.09 / 312.58 | 3.28 |
| 100,000 | 1 | 101.39 / 123.93 | 102.09 / 133.93 | 210.13 / 241.52 | 419.51 / 450.43 | 630.88 / 667.20 | 1.17 |
| 100,000 | 5 | 101.47 / 117.22 | 102.55 / 108.90 | 208.35 / 213.95 | 412.12 / 470.76 | 1454.02 / 1527.75 | 0.60 |
| 100,000 | 20 | 98.88 / 110.14 | 99.68 / 103.02 | 205.59 / 219.21 | 408.30 / 448.02 | 4506.59 / 4834.94 | 0.21 |

## Interpretation and recommended follow-up

The 100k-event samples show both reading/deserializing and the in-memory fold taking substantial time; neither measurement alone explains all HTTP latency. The increase in fold cost is consistent with the current turn projection scanning historical turns in each session for start and end events (`projection.rs`), but this benchmark is not a profiler and does not prove attribution. The service currently reloads and folds during append and again while deriving completion; each live subscriber independently recomputes a snapshot behind the shared service mutex. Those implementation facts are consistent with the growing last-subscriber delivery times.

First, profile the long-history case and replace repeated scans of historical turns with an index of active and provisional turns, retaining the existing semantics for late and out-of-order events. Add equivalence tests covering reordered starts and ends, overlapping sessions, equal timestamps, retries, and replay. Then measure avoiding redundant append/completion reads and sharing snapshot computation among subscribers with compatible requests. Repeat this exact matrix, add an open-loop load test and multiple active projects, and compare correctness and memory alongside latency.

Only then choose whether an incremental or persistent projection cache is justified. Any shared or cached result must account for source generation, cursor, non-event verification and other state changes, request window and time-dependent `as_of`/clock values; cursor alone is not a complete invalidation key. Keep the event log authoritative and define recovery, consistency, and differential replay tests before introducing derived state.
