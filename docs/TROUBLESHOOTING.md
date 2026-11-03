# Troubleshooting

These commands describe the current source checkout; the published v0.2.0
release does not include `capture`, `local-test`, or `outbox rejected`.

- **No prompt captured:** In the project directory, run `speedrun capture` and
  follow its next step. Check that the chosen host adapter is installed, restart
  that host, send a new prompt in this project, and run `speedrun capture` again.
  A captured state confirms that a prompt arrived; Zed's native agent does not
  currently provide prompt capture.
- **Project is untrusted:** In the project directory, run `speedrun setup` and
  confirm the trust prompt. Run `speedrun capture` again; it should report the
  project as trusted. Only trust a project you recognize.
- **Daemon answers for another data directory:** Run `speedrun doctor` and
  compare the data directory and address with the intended `SPEEDRUN_HOME` and
  `SPEEDRUN_ADDR`. Use the correct environment or a separate address for each
  data directory, then retry. The client will not send events to the wrong daemon.
- **Writes are queued while offline:** Restore daemon access, then run
  `speedrun flush` and check `speedrun capture` or `speedrun doctor` for the
  pending count. Pending writes are retried; permanent rejections are not.
- **Requests were rejected:** Run `speedrun outbox rejected` to inspect retained
  status and rejection times, and `speedrun doctor` for the count. Correct the
  underlying project or request problem and submit a new action if appropriate.
  Rejected entries are diagnostics, not a queue that `flush` replays; their
  bodies and daemon error messages are not shown.
- **Local test cannot verify a dirty tree:** Run `git status`, ignore generated files and commit the intended changes, then run
  `speedrun local-test -- <command> [args...]` again, then use
  `speedrun milestone` to attach the new passing observation. It must match the
  current Git HEAD and meet the milestone's verification requirements.
- **Evidence verification fails or is unavailable:** Inspect the report with
  `speedrun verification <event-id>` using the event ID returned by the claim or
  evidence job, or use the editor's **Inspect Local Evidence Check** action. Correct the
  evidence or restore access to the relevant verifier, then attach corrected or
  new evidence through `speedrun milestone <id>`. Attaching evidence or making
  a claim alone does not guarantee verification or that the milestone counts.
