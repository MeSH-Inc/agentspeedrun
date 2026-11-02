# Releasing

One version covers everything shipped from this repo: the Rust crates, the
VS Code extension, the Zed extension, the opencode and pi packages and the
Claude Code plugin. `scripts/version.py check` (run in CI) fails if they drift.

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

The `release` workflow then:

- checks the tag matches the manifests;
- runs `cargo fmt --check`, clippy and `cargo test --all --locked`; nothing is
  built or published from a commit whose tests fail;
- builds `speedrun` and `speedrund` for `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl` (cross-built with zig), `aarch64-apple-darwin`
  and `x86_64-apple-darwin`, and runs `scripts/smoke.sh` on each (the arm64
  Linux build under QEMU): daemon autostart, a run, a verified split, the hash
  chain, the embedded adapters;
- packages each as `agentspeedrun-vX.Y.Z-<target>.tar.gz` (`scripts/package.sh`).
  Archives are reproducible (`scripts/archive.py`): entries in name order,
  owned by 0:0, dated by the tagged commit (`SOURCE_DATE_EPOCH`), and a gzip
  header with no name or time, so packaging the same binaries twice gives the
  same sha256. The toolchain is pinned in `rust-toolchain.toml`;
- builds and tests the `.vsix` in a real VS Code, and builds the Zed extension;
- writes `SHA256SUMS`, installs from those exact files with `install.sh` and
  smoke-tests the result;
- attests build provenance for every archive and the `.vsix`
  (`actions/attest-build-provenance`), which `install.sh` checks with
  `gh attestation verify` when an authenticated `gh` is present. Attestations
  need a public repository on the organization's plan;
- publishes a GitHub release with generated notes. Tags with a `-` (e.g.
  `v0.2.0-rc.1`) become prereleases.

Run the workflow by hand from the Actions tab (or `gh workflow run release.yml`)
for a dry run that does everything except attest and publish.

Every action in both workflows is pinned by commit, with the release named in
a comment; update one by resolving its new tag with
`gh api repos/<owner>/<action>/commits/<tag> --jq .sha`. On every push, CI also
runs the hooks in `plugins/claude-code/hooks/hooks.json` against the last
released binary (`scripts/hooks_against_release.sh`), since the plugin's
hooks reach users from `main` before the binary they call does.

## Distribution channels

| Channel | State |
|---------|-------|
| GitHub release + `install.sh` | Automated. |
| Claude Code plugin | This repo's marketplace (`.claude-plugin/marketplace.json`) serves the plugin from the release tag; `scripts/version.py set` moves the pin. A plugin from a tag is fetched only when it is enabled in user settings, not in a committed project `.claude/settings.json`. |
| opencode, pi, oh-my-pi | Embedded in `speedrun` (`speedrun setup …`); pi also installs from git. |
| VS Code | `.vsix` attached to each release. Marketplace/Open VSX publishing needs a publisher account and token (`VSCE_PAT`, `OVSX_PAT`). |
| Zed | Not in the registry yet: submission is a PR to `zed-industries/extensions` and requires a public repo with an open-source license. |
