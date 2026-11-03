#!/usr/bin/env python3
"""Exercise compiled CLI/daemon recovery with concurrent hooks in isolated data.

Run after cargo build: python3 scripts/recovery-smoke.py [binary-directory]
"""

import concurrent.futures
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time


def main():
    binaries = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug").resolve()
    cli, daemon = binaries / "speedrun", binaries / "speedrund"
    assert cli.is_file() and daemon.is_file(), "Run cargo build first"
    with tempfile.TemporaryDirectory(prefix="speedrun-recovery-smoke-") as temporary:
        root = Path(temporary)
        project = root / "project"
        project.mkdir()
        source, bundle, restored = (root / name for name in ("source", "bundle", "restored"))
        environment = os.environ.copy()
        for name in ("SPEEDRUN_ADDR", "SPEEDRUN_BIN", "SPEEDRUN_COMMONS_URL", "XDG_DATA_HOME"):
            environment.pop(name, None)
        environment.update(HOME=str(root), SPEEDRUN_NO_AUTOSTART="1")
        processes = []

        def run(home, *args, body=""):
            result = subprocess.run(
                [str(cli), *map(str, args)], input=body, text=True,
                cwd=project, env={**environment, "SPEEDRUN_HOME": str(home)},
                capture_output=True, timeout=30,
            )
            assert result.returncode == 0, (args, result.stdout, result.stderr)
            return result.stdout

        def start(home):
            process = subprocess.Popen(
                [str(daemon)], cwd=project,
                env={**environment, "SPEEDRUN_HOME": str(home), "SPEEDRUN_ADDR": "127.0.0.1:0"},
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            processes.append(process)
            deadline = time.monotonic() + 10
            while not (home / "addr").exists():
                assert process.poll() is None, "daemon exited during startup"
                assert time.monotonic() < deadline, "daemon startup timed out"
                time.sleep(0.02)
            return process

        def hook(number):
            run(source, "hook", "claude-code", "user-prompt-submit", body=json.dumps({
                "prompt": f"recovery fixture {number}", "session_id": f"session-{number}",
            }))

        def queue(home):
            with sqlite3.connect(home / "spool/pending.sqlite3") as db:
                return {json.loads(entry)["body"]["event_id"] for (entry,) in
                        db.execute("SELECT entry FROM outbox")}

        try:
            subprocess.run(["git", "init", "-q"], cwd=project, check=True)
            live = start(source)
            run(source, "init")
            initial = json.loads(run(source, "ingest", "prompt", "--host", "smoke", "--session", "initial", body="start recovery fixture"))
            assert initial["recorded"] and not initial["spooled"]
            live.terminate()
            live.wait(timeout=10)

            hook("before")
            before = queue(source)
            assert len(before) == 1
            # Model a dead delivery owner whose lease has expired.
            with sqlite3.connect(source / "spool/pending.sqlite3") as db:
                db.execute("UPDATE outbox SET state='leased', owner='dead-owner', lease_until=0")
            with concurrent.futures.ThreadPoolExecutor(max_workers=5) as pool:
                writers = [pool.submit(hook, i) for i in range(12)]
                recovery = pool.submit(run, source, "recovery", "create", bundle)
                recovery.result()
                for writer in writers:
                    writer.result()
            included = queue(bundle)
            accepted = queue(source)
            assert before <= included <= accepted and len(accepted) == 13
            hook("after")
            source_after = queue(source)
            assert len(source_after) == 14
            run(source, "recovery", "restore", bundle, restored)
            assert queue(restored) == included
            assert not (restored / "token").exists(), "recovery copied bearer token"
            recovered = start(restored)
            run(restored, "flush")
            run(restored, "flush")
            assert not queue(restored), "restored requests were not acknowledged"
            with sqlite3.connect(restored / "speedrun.sqlite") as db:
                counts = dict(db.execute("SELECT event_id, COUNT(*) FROM events GROUP BY event_id"))
                assert all(counts.get(event_id) == 1 for event_id in included)
                assert not (accepted - included) & counts.keys(), "restored post-cutoff request"
            assert queue(source) == source_after, "source queue changed during restore/replay"
            proof = run(restored, "verify").strip()
            assert proof.startswith("ok"), proof
            recovered.terminate()
            recovered.wait(timeout=10)
            print(f"Recovery smoke passed: 14 accepted hooks, {len(included)} snapshotted, expired lease replayed exactly once; {proof}")
        finally:
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=10)


if __name__ == "__main__":
    main()
