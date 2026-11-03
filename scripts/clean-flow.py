#!/usr/bin/env python3
"""Exercise guided onboarding through a completed run with isolated user data.

Usage: scripts/clean-flow.py <directory containing speedrun and speedrund>
The binaries must already be built; no host agent is required. The installed
Copilot hook command is invoked with the same JSON input a host supplies.
"""
import json
import os
from pathlib import Path
import pty
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    bindir = Path(sys.argv[1]).resolve()
    binary = bindir / "speedrun"
    if not binary.is_file() or not (bindir / "speedrund").is_file():
        sys.exit("Expected speedrun and speedrund in " + str(bindir))
    with tempfile.TemporaryDirectory(prefix="speedrun clean flow ") as tmp:
        root = Path(tmp)
        project = root / "project with spaces"
        project.mkdir()
        home = root / "home"
        home.mkdir()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            addr = "127.0.0.1:" + str(sock.getsockname()[1])
        env = {key: value for key, value in os.environ.items()
               if not key.startswith("SPEEDRUN_") and key not in
               {"COPILOT_HOME", "PYTHONDONTWRITEBYTECODE", "PYTHONPYCACHEPREFIX"}}
        env.update(HOME=str(home), SPEEDRUN_HOME=str(root / "data"),
                   XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "xdg-data"),
                   SPEEDRUN_ADDR=addr, GIT_CONFIG_NOSYSTEM="1",
                   PATH=str(bindir) + os.pathsep + env.get("PATH", ""))

        def run(*args, input=None, check=True):
            result = subprocess.run([str(binary), *args], cwd=project, env=env,
                                    input=input, text=True, capture_output=True, timeout=30)
            if check and result.returncode:
                raise AssertionError(f"{args}: exit {result.returncode}\n{result.stdout}\n{result.stderr}")
            return result

        def interactive(args, answers, expected=0):
            master, slave = pty.openpty()
            proc = subprocess.Popen([str(binary), *args], cwd=project, env=env,
                                    stdin=slave, stdout=slave, stderr=slave, close_fds=True)
            os.close(slave)
            os.write(master, answers.encode())
            output = bytearray()
            deadline = time.monotonic() + 30
            try:
                while time.monotonic() < deadline:
                    ready, _, _ = select.select([master], [], [], 0.1)
                    if ready:
                        try:
                            data = os.read(master, 65536)
                        except OSError:
                            break
                        if not data:
                            break
                        output.extend(data)
                    if proc.poll() is not None and not ready:
                        break
                else:
                    proc.kill()
                    raise AssertionError(f"interactive command timed out: {args}\n{output.decode(errors='replace')}")
                code = proc.wait(timeout=5)
                text = output.decode(errors="replace")
                if code != expected:
                    raise AssertionError(f"{args}: exit {code}, expected {expected}\n{text}")
                return text
            finally:
                os.close(master)
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()

        def readiness():
            return json.loads(run("readiness", "--json").stdout)

        try:
            subprocess.run(["git", "init", "-q"], cwd=project, env=env, check=True)
            subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                            "commit", "-q", "--allow-empty", "-m", "initial"], cwd=project, env=env, check=True)
            assert readiness()["project"] == "untracked"
            cancelled = interactive(["setup"], "copilot\nproject\n\ncontent\nn\n", expected=1)
            assert "setup cancelled" in cancelled
            assert readiness()["project"] == "untracked"
            assert not (project / ".github/hooks/agentspeedrun.json").exists()
            setup = interactive(["setup"], "copilot\nproject\n\ncontent\ny\n")
            assert "prompt capture has not been verified" in setup
            before = readiness()
            assert before["project"] == "trusted" and before["daemon"] == "offline", before
            run("flush")
            before = readiness()
            assert before["recording"] == "awaiting_prompt" and before["integration_state"] == "installed", before
            hook = project / ".github/hooks/agentspeedrun.json"
            config = json.loads(hook.read_text())
            command = config["hooks"]["UserPromptSubmit"][0]["bash"]
            result = subprocess.run(command, shell=True, cwd=project, env=env, text=True,
                                    input=json.dumps({"cwd": str(project), "sessionId": "clean-flow",
                                                      "prompt": "Build the requested demo."}),
                                    capture_output=True, timeout=30)
            assert result.returncode == 0, result.stderr
            after = readiness()
            assert after["recording"] == "captured" and after["last_host"] == "copilot", after
            hook.unlink()
            missing_after_capture = readiness()
            assert missing_after_capture["recording"] == "captured" and missing_after_capture["next_action_id"] == "setup", missing_after_capture
            hook.write_text(json.dumps(config))
            interactive(["milestone", "scaffold-runs"], "1\n")
            interactive(["milestone", "happy-path-demo"], "1\n")
            status = json.loads(run("status", "--json").stdout)
            assert status["status"] == "completed", status
            history = json.loads(run("history", "--json").stdout)
            assert history, history
            run("daemon", "restart")
            assert json.loads(run("history", "--json").stdout) == history
            assert json.loads(run("status", "--json").stdout)["status"] == "completed"
            # A second project performs a real edit and local verification. The
            # hundred-percent category permits local test evidence, while still
            # requiring deployment for the full product run.
            project = root / "small calculator"
            project.mkdir()
            subprocess.run(["git", "init", "-q"], cwd=project, env=env, check=True)
            (project / "calculator.py").write_text("def add(a, b):\n    return a - b\n")
            (project / "test_calculator.py").write_text(
                "import unittest\nfrom calculator import add\n"
                "class CalculatorTest(unittest.TestCase):\n"
                "    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n"
            )
            run("setup", "--host", "copilot", "--scope", "project", "--category",
                "hundred-percent", "--capture", "metadata-only", "--yes")
            subprocess.run(["git", "add", "."], cwd=project, env=env, check=True)
            subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                            "commit", "-qm", "add calculator"], cwd=project, env=env, check=True)
            run("ingest", "prompt", "--host", "copilot", "--session", "calculator",
                input="Fix calculator addition and test it.")
            failed = run("local-test", "--", "python3", "-m", "unittest", check=False)
            assert failed.returncode != 0 and "test command failed" in failed.stderr, (failed.stdout, failed.stderr)
            dirty = run("local-test", "--", "python3", "-m", "unittest", check=False)
            assert dirty.returncode != 0 and "git status" in dirty.stderr, dirty.stderr
            shutil.rmtree(project / "__pycache__")
            (project / ".gitignore").write_text("__pycache__/\n")
            (project / "calculator.py").write_text("def add(a, b):\n    return a + b\n")
            subprocess.run(["git", "add", "calculator.py", ".gitignore"], cwd=project, env=env, check=True)
            subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                            "commit", "-qm", "fix addition"], cwd=project, env=env, check=True)
            passed = run("local-test", "--", "python3", "-m", "unittest")
            assert "success" in passed.stdout.lower(), passed.stdout
            run("ingest", "turn-end", "--host", "copilot", "--session", "calculator")
            run("pause")
            run("daemon", "restart")
            paused = json.loads(run("status", "--json").stdout)
            assert paused["status"] == "paused", paused
            run("resume")
            result = interactive(["milestone", "tests-green"], "1\n1\n1\n")
            assert "passed" in result and "current HEAD" in result, result
            assert "verified" in result.lower(), result
            verified = json.loads(run("milestone", "tests-green", "--json").stdout)
            assert verified["milestones"][0]["counts"], verified
            run("verify")
            # Return to the completed project for the reset/removal check.
            project = root / "project with spaces"
            # Start a new run with no prompt, and check guidance after removal.
            run("reset", "--yes")
            hook.unlink()
            missing = readiness()
            assert missing["recording"] == "awaiting_prompt" and missing["integration_state"] == "missing", missing
            assert missing["next_action"] == "Install an integration with speedrun setup.", missing
            print("clean flow: ok (guided setup, real code edit, local verification, pause/restart/resume and completion)")
        finally:
            run("daemon", "stop", check=False)


if __name__ == "__main__":
    main()
