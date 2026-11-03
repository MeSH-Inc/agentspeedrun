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
  each platform. Package each pair with `scripts/package.sh <target> <bindir>`.
  Archives are reproducible: entries are sorted, owned by 0:0 and dated by
  the tagged commit (or `SOURCE_DATE_EPOCH`).
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
Same-version replacement tests the installer and daemon restart, not compatibility
between releases. Run the lifecycle test on each supported platform before a
release; it does not exercise other architectures or build attestations.

For the VS Code package, run `scripts/vsix-lifecycle.sh /path/to/code` with a
local VS Code CLI. It builds a VSIX and installs, replaces, lists and removes
it using isolated profile and extension directories. This checks package
lifecycle separately from the real VS Code host integration tests.
