# Startup and outbox smoke test

Run `scripts/startup-smoke.sh <bindir>` against a directory containing the
packaged `speedrun` and `speedrund` binaries. It runs the ordinary cold-start
smoke with two CPU workers, then repeats it with a copied daemon delayed by two
seconds. The second run requires the first prompt to be spooled, flushes it once
the original daemon is ready, flushes again, and checks that the run has one
prompt and one session. Each smoke uses a temporary data directory and port, and the
harness stops its load workers on exit. The two-second delay intentionally
exceeds the client's 1.5-second autostart polling window; it is a deterministic
way to exercise recovery, not a measurement of real daemon startup.

On 2026-11-02, three runs of the 0.3.1 native macOS package under two CPU
workers completed the full cold-start smoke in 1.29–1.34 seconds. Three runs
with the artificial two-second daemon delay completed in 3.66–3.71 seconds,
and the delayed first prompt was delivered once after a flush. A prior Rosetta
release smoke had returned a spooled first prompt under concurrent build and
test activity; a retry passed. These observations explain why the smoke must
allow durable replay, but do not establish the cause of that earlier delay or
represent a startup latency guarantee.

The same harness also passed against the packaged x86_64 macOS 0.3.1 binaries
under Rosetta: both cold startup and the forced delayed replay completed under
two CPU workers. This does not reproduce the full earlier build workload.
