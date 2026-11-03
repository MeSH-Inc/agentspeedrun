# Releasing

One version covers everything shipped from this repo: the Rust crates, the
VS Code extension, the Zed extension, the opencode and pi packages and the
Claude Code plugin. `scripts/version.py check` fails if they drift.

1. Bump every manifest and the lockfiles. This also pins the Claude Code
   plugin in `.claude-plugin/marketplace.json` to the new tag (a `git-subdir`
   source with `ref: vX.Y.Z`), so a hooks change on `main` reaches users only
   with the release whose binary handles it:

   ```bash
   scripts/version.py set 0.2.0
   ```

2. Commit, then tag and push:

   ```bash
   git commit -am "Release 0.2.0" && git tag v0.2.0 && git push origin main v0.2.0
   ```

Before publishing, verify locally:

- Run `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --all --locked`, and `scripts/version.py check`.
- Build the binaries for each supported target and run `scripts/smoke.sh` on
  each platform. Package each pair with `scripts/package.sh <target> <bindir>`,
  extract the archive and run `scripts/clean-flow.py <extracted-bindir>` to
  verify the guided setup and capture journey. Archives are reproducible:
  entries are sorted, owned by 0:0 and dated by the tagged commit (or
  `SOURCE_DATE_EPOCH`). On Linux, run the extracted binaries on a system with a
  process reaper; for containers, use `podman run --init` or `docker run --init`
  so daemon restarts can observe that the previous process has exited.
- Build and test the VS Code extension and Zed extension as described in
  `CLAUDE.md`. Generate `SHA256SUMS` for the release assets and run the
  packaged lifecycle verification below on each available platform.
- Publish the verified assets and checksums. If build attestations are
  available, `install.sh` verifies them when an authenticated `gh` is present.

## Distribution channels

| Channel | State |
|---------|-------|
| GitHub release + `install.sh` | Release archives and checksums. |
| Claude Code plugin | This repo's marketplace (`.claude-plugin/marketplace.json`) serves the plugin from the release tag; `scripts/version.py set` moves the pin. A plugin from a tag is fetched only when it is enabled in user settings, not in a committed project `.claude/settings.json`. |
| opencode, pi, oh-my-pi | Embedded in `speedrun` (`speedrun setup …`); pi also installs from git. |
| VS Code | `.vsix` attached to each release. Marketplace/Open VSX publishing needs a publisher account and token (`VSCE_PAT`, `OVSX_PAT`). |
| Zed | Not in the registry yet: submission is a PR to `zed-industries/extensions` and requires a public repo with an open-source license. |

## Local packaged lifecycle verification

After building the native binaries, run `scripts/packaged-lifecycle.sh target/debug`.
It creates a release-shaped archive from those binaries, serves it over loopback
HTTP and exercises installation, same-version replacement, capture, removal,
reinstallation and checksum rejection under temporary HOME, XDG and data paths.
The installer identifies an existing daemon through `/proc` on Linux and uses
`ps` elsewhere. The v0.2.0 release needs the external `kill` command for daemon
stop; install the OS process tools when testing that baseline on minimal images.
Same-version replacement tests the installer and daemon restart, not compatibility
between releases. Run the lifecycle test on each supported platform before a
release; it does not exercise other architectures or build attestations.

For the VS Code package, run `scripts/vsix-lifecycle.sh /path/to/code` with a
local VS Code CLI. It builds a VSIX and installs, replaces, lists and removes
it using isolated profile and extension directories. This checks package
lifecycle separately from the real VS Code host integration tests.

## Published release upgrade verification

Test the published v0.2.0 archive against the development binaries on each
platform. Download the archive and `SHA256SUMS` from the release, then run:

```bash
scripts/release-upgrade.sh /path/to/agentspeedrun-v0.2.0-TARGET.tar.gz /path/to/SHA256SUMS target/debug
```

The script verifies the published checksum, installs that archive, creates a
trusted project with signed history and queues a prompt while offline. It then
installs a separately built development archive and checks the existing history,
trust, signature chain and delivery of the old outbox. It compares an existing
preferences file if the baseline created one. This harness expects the v0.2.0
legacy JSONL outbox; update it when testing a newer release. Use binaries for
the named target; the checkout version may match the published tag while the
code differs.

The v0.2.0 release predates capture preferences, so it cannot test preservation
of a preference set through that release. The earlier v0.1 format is retired;
see `docs/V0.2.md` §5. A successful upgrade from v0.2.0 does not establish v0.1
compatibility. Record the archive hash, checkout revision and platform with each
verification.
