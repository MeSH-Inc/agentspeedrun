# Releasing

One version covers everything shipped from this repo: the Rust crates, the
VS Code extension, the Zed extension, the opencode and pi packages and the
Claude Code plugin. `scripts/version.py check` fails if they drift.

Local verification requires the pinned Rust toolchain, Python 3.11 or newer
(for version and lockfile checks), Node.js, Bun, and the adapter dependencies.

1. Set the version with `scripts/version.py set X.Y.Z`. This updates manifests,
   lockfiles, and the Claude Code marketplace pin to `vX.Y.Z`.
2. Commit the candidate and run the local checks below. Record their results and
   asset hashes before creating a tag or publishing.
3. For the prepared 0.3.3 preview, use tag `v0.3.3`, mark the GitHub release
   as a prerelease explicitly, and package the VS Code extension with
   `vsce package --pre-release`. A numeric version keeps the extension
   manifests compatible; the tag alone does not identify it as a preview.
   The existing tag-triggered release workflow only infers prerelease status
   from a hyphenated tag, so do not rely on it for this preview.

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

## Platform verification

The installer selects one of the following release archive targets. A target
existing in a release is not evidence that the current checkout was tested on
that platform. Record the commit, archive checksum, operating system and whether
execution was native or emulated for each release candidate.

| OS and CPU | Archive target |
|------------|----------------|
| macOS Apple silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Linux arm64 | `aarch64-unknown-linux-musl` |
| Linux x86_64 | `x86_64-unknown-linux-musl` |

Record candidate-specific runtime coverage and limitations in the release's
`VERIFICATION.md`, alongside the commit and asset checksums.

Cross-compilation alone does not exercise the installer, daemon, or host process
handling. When a required runtime is unavailable, record that gap with the
release candidate rather than claiming runtime coverage. Linux containers must
use a process reaper as described above.

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

Test the most recent published archive and the v0.2.0 archive against the
development binaries on each platform. Download each archive and its release's
`SHA256SUMS`, then run the following for each baseline:

```bash
scripts/release-upgrade.sh /path/to/agentspeedrun-v0.3.2-TARGET.tar.gz /path/to/SHA256SUMS target/debug
scripts/release-upgrade.sh /path/to/agentspeedrun-v0.2.0-TARGET.tar.gz /path/to/SHA256SUMS target/debug
```

The script verifies the published checksum, installs that archive, creates a
trusted project with signed history and queues a prompt while offline. It then
installs a separately built development archive and checks the existing history,
trust, signature chain and delivery of the old outbox. It covers the v0.2
legacy JSONL outbox and v0.3 or later SQLite outbox. For v0.3 or later it also
sets non-default capture and retention preferences and installs the opencode
user integration, then verifies those preferences and the selected integration's
installed readiness before and after upgrading. All settings are isolated in a
temporary home. Use binaries for the named target; the checkout version may
match the published tag while the code differs.

The v0.2.0 release predates capture preferences, so it cannot test preservation
of a preference set through that release. The earlier v0.1 format is retired;
see `docs/V0.2.md` §5. A successful upgrade from v0.2.0 does not establish v0.1
compatibility. Record the archive hash, checkout revision and platform with each
verification.

## When to cut the next release

Publish a new preview when a verified user-facing fix changes the installed
binary or embedded adapters, or when a new capability is ready for outside
use. Run the candidate checks above and ship matching binaries and docs.
Verification-only scripts and coverage records can be pushed without replacing
the current preview.

Before calling a release stable, exercise clean installation and setup, actual
prompt capture, normal turn completion, and a model-invoked milestone tool in
each advertised capture host. Record host versions, authentication/provider
blockers, native versus emulated platforms, and unsupported capabilities.
Retain preview status while important advertised paths remain unverified.
A provider authentication failure is a coverage gap, not evidence that the
adapter works or that it is broken. Do not relabel fixture-based tool tests as
live-host verification.

### Remaining promotion evidence after the v0.3.2 follow-ups

Use `scripts/live-host.py` for repeatable CLI-host checks and
`docs/FIRST_USE_TESTING.md` for the limits of the agent-led first-use sessions.
Before promoting the next candidate:

- Check the supported VS Code Local agent path with the installed candidate
  extension/hooks: real prompt, completed turn, and milestone tool call. The
  v0.3.3 work verified this path; see `docs/VS_CODE_LIVE_VERIFICATION.md` for
  the exact artifact coverage. The follow-up also verified the Copilot SDK Agent Host after MCP startup
  finished; retain the documented distinction between interrupted startup and
  completed model/tool checks.
- Observe a fresh user completing setup, capture, a milestone, and a recovery
  action without coaching. Agent-led and synthetic-hook sessions are separate
  evidence, not a substitute for that usability observation.
- Freeze the candidate, then verify its packaged installer/removal, upgrades,
  recovery and host checks against its exact binaries. The source help fix
  after v0.3.2 requires rebuilt artifacts if it is included in the release.

Native Intel hardware remains unavailable; disclose emulated coverage or narrow
platform claims instead of claiming a native pass. Treat Zed native prompt
capture as unsupported and keep Zed model/tool coverage explicit. The disclosed
OpenCode hot-reload limitation requires a host restart after adapter upgrades;
verify ordinary restart/setup, and avoid presenting untested hot reload as
supported. Build attestations remain absent; checksums and event signatures
serve different purposes. No new CI or signing process is required merely to
promote this greenfield tool, but these distribution limits must stay visible.
