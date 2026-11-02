#!/usr/bin/env python3
"""Keep every shipped version in step with the Cargo workspace.

  scripts/version.py get            print the workspace version
  scripts/version.py check [vX.Y.Z] fail if any manifest differs (or the tag does)
  scripts/version.py set X.Y.Z      rewrite every manifest, then run cargo to refresh Cargo.lock

The Claude Code marketplace pins the plugin to the release tag (V0.2 M1), so
`set` also points its source at vX.Y.Z, and `check` fails when it does not.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$")

# TOML files: the first `version = "..."` line is the package's own version.
TOML = ["Cargo.toml", "plugins/zed/Cargo.toml", "plugins/zed/extension.toml"]
# JSON files: the top-level "version" key.
JSON = [
    "plugins/vscode/package.json",
    "plugins/opencode/package.json",
    "plugins/pi/package.json",
    "plugins/claude-code/.claude-plugin/plugin.json",
]
# The marketplace's plugin entry, fetched from this repository at the release tag.
MARKETPLACE = ".claude-plugin/marketplace.json"
PLUGIN_PATH = "plugins/claude-code"
REPOSITORY_URL = "https://github.com/MeSH-Inc/agentspeedrun.git"
TOML_VERSION = re.compile(r'^version = "([^"]+)"', re.M)
JSON_VERSION = re.compile(r'^(  "version": ")([^"]+)(")', re.M)


def versions():
    for f in TOML:
        m = TOML_VERSION.search((ROOT / f).read_text())
        yield f, m and m.group(1)
    for f in JSON:
        yield f, json.loads((ROOT / f).read_text()).get("version")
    yield MARKETPLACE, pinned_version()


def plugin_source(version):
    return {
        "source": "git-subdir",
        "url": REPOSITORY_URL,
        "path": PLUGIN_PATH,
        "ref": f"v{version}",
    }


def pinned_version():
    """The version the marketplace's plugin source is pinned to, if it is pinned."""
    plugins = json.loads((ROOT / MARKETPLACE).read_text())["plugins"]
    source = plugins[0]["source"] if len(plugins) == 1 else None
    if not isinstance(source, dict):
        return None
    ref = source.get("ref", "")
    expected = plugin_source(ref.removeprefix("v"))
    return ref.removeprefix("v") if source == expected else None


def get():
    return dict(versions())["Cargo.toml"]


def check(tag=None):
    want = get()
    bad = [f"  {f}: {v}" for f, v in versions() if v != want]
    if tag and tag != f"v{want}":
        bad.append(f"  tag {tag} does not match v{want}")
    if bad:
        sys.exit(f"version mismatch (workspace is {want}):\n" + "\n".join(bad))
    print(f"all manifests at {want}")


def set_version(new):
    if not SEMVER.match(new):
        sys.exit(f"{new} is not X.Y.Z")
    for f in TOML:
        p = ROOT / f
        p.write_text(TOML_VERSION.sub(f'version = "{new}"', p.read_text(), count=1))
    for f in JSON:
        p = ROOT / f
        text, n = JSON_VERSION.subn(rf"\g<1>{new}\g<3>", p.read_text(), count=1)
        if n != 1:
            sys.exit(f"{f}: no top-level version line")
        p.write_text(text)
    p = ROOT / MARKETPLACE
    marketplace = json.loads(p.read_text())
    if len(marketplace["plugins"]) != 1:
        sys.exit(f"{MARKETPLACE}: expected one plugin")
    marketplace["plugins"][0]["source"] = plugin_source(new)
    p.write_text(json.dumps(marketplace, indent=2) + "\n")
    # Refresh the lockfiles' own entries without touching dependencies.
    for d in [ROOT, ROOT / "plugins/zed"]:
        subprocess.run(["cargo", "update", "--workspace", "-q"], cwd=d, check=True)
    for d in ["plugins/vscode", "plugins/opencode", "plugins/pi"]:
        lock = ROOT / d / "package-lock.json"
        if lock.exists():
            data = json.loads(lock.read_text())
            data["version"] = new
            data.get("packages", {}).get("", {})["version"] = new
            lock.write_text(json.dumps(data, indent=2) + "\n")
    check()


if __name__ == "__main__":
    cmd, *args = sys.argv[1:] or ["get"]
    if cmd == "get":
        print(get())
    elif cmd == "check":
        check(*args)
    elif cmd == "set" and len(args) == 1:
        set_version(args[0])
    else:
        sys.exit(__doc__)
