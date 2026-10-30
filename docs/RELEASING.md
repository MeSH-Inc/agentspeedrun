# Releasing

One version covers everything shipped from this repo: the Rust crates, the
VS Code extension, the Zed extension, the opencode and pi packages and the
Claude Code plugin. `scripts/version.py check` (run in CI) fails if they drift.

1. Bump every manifest and the lockfiles:

   ```bash
   scripts/version.py set 0.2.0
   ```

2. Commit, then tag and push:

   ```bash
   git commit -am "Release 0.2.0" && git tag v0.2.0 && git push origin main v0.2.0
   ```

The `release` workflow then:

- checks the tag matches the manifests;
- builds `speedrun` and `speedrund` for `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl` (cross-built with zig), `aarch64-apple-darwin`
  and `x86_64-apple-darwin`, and runs `scripts/smoke.sh` on each (the arm64
  Linux build under QEMU): daemon autostart, a run, a verified split, the hash
  chain, the embedded adapters;
- packages each as `agentspeedrun-vX.Y.Z-<target>.tar.gz` (`scripts/package.sh`);
- builds and tests the `.vsix` in a real VS Code, and builds the Zed extension;
- writes `SHA256SUMS`, installs from those exact files with `install.sh` and
  smoke-tests the result;
- publishes a GitHub release with generated notes. Tags with a `-` (e.g.
  `v0.2.0-rc.1`) become prereleases.

Run the workflow by hand from the Actions tab (or `gh workflow run release.yml`)
for a dry run that does everything except publish.

## Distribution channels

| Channel | State |
|---------|-------|
| GitHub release + `install.sh` | Automated. Anonymous downloads need a public repo; until then `install.sh` downloads through `gh`. |
| Claude Code plugin | Served from this repo's marketplace (`.claude-plugin/marketplace.json`); nothing to publish. |
| opencode, pi, oh-my-pi | Embedded in `speedrun` (`speedrun setup …`); pi also installs from git. |
| VS Code | `.vsix` attached to each release. Marketplace/Open VSX publishing needs a publisher account and token (`VSCE_PAT`, `OVSX_PAT`). |
| Zed | Not in the registry yet: submission is a PR to `zed-industries/extensions` and requires a public repo with an open-source license. |
