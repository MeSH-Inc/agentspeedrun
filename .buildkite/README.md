# Fleet verification

`bash .buildkite/verify.sh` runs the existing Rust, adapter, VS Code, Zed,
packaging and released-hook checks in a pinned Linux container. Stages run
independently so one failure does not hide later failures. The wrapper bounds
CPU and memory and removes the per-job Compose resources on exit.
Rust test cases run one at a time on the bounded runner; tests that exercise
concurrency still start their own workers. Adapter tests have a 30-second test
timeout to allow their CLI and daemon subprocesses to finish on slower disks.

The requested PR head is merged against its fetched target branch before
verification. The shared fleet queue accepts trusted same-repository sources
only and does not accept third-party forks. The tag-triggered GitHub release
workflow remains separate: its cross-platform artifacts and GitHub provenance
must not be treated as migrated by this verification pipeline.
