#!/bin/sh
# Install the speedrun CLI and daemon from a GitHub release of MeSH-Inc/agentspeedrun,
# on Linux or macOS (Windows is not supported; use WSL).
#
#   curl -fsSL https://raw.githubusercontent.com/MeSH-Inc/agentspeedrun/main/install.sh | sh
#
# With an authenticated GitHub CLI the script downloads through `gh`, and
# checks the archive's build attestation with `gh attestation verify`.
#
# Environment:
#   SPEEDRUN_VERSION      release tag to install, e.g. v0.1.0 (default: latest)
#   SPEEDRUN_INSTALL_DIR  where to put the binaries (default: ~/.local/bin)
#   SPEEDRUN_DOWNLOAD_URL base URL holding the release assets, for mirrors
#                         and tests (needs SPEEDRUN_VERSION)
#   SPEEDRUN_SKIP_ATTESTATION=1  skip the attestation check, for archives built
#                         outside the release workflow (tests)
set -eu

repo="MeSH-Inc/agentspeedrun"
version="${SPEEDRUN_VERSION:-}"
dest="${SPEEDRUN_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'speedrun-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

# Only act on the daemon authenticated by this data directory and launched
# from this installation. An unrelated daemon may share its executable name.
matching_daemon() {
  [ -x "$dest/speedrun" ] && [ -d "$dest" ] || return 1
  canonical_dest=$(cd "$dest" && pwd -P)
  logical_dest=$(cd "$dest" && pwd -L)
  status=$("$dest/speedrun" daemon status 2>/dev/null) || return 1
  pid=$(printf '%s\n' "$status" | sed -n 's/.* pid \([0-9][0-9]*\) at .*/\1/p')
  [ -n "$pid" ] || return 1
  if [ "$(uname -s)" = Linux ] && [ -L "/proc/$pid/exe" ]; then
    command=$(readlink "/proc/$pid/exe" 2>/dev/null) ||
      die "could not inspect daemon process $pid in /proc; binaries left untouched"
    # Under binary translation /proc/PID/exe can name the interpreter. The
    # authenticated daemon's first argument still names its launched binary.
    if [ "$command" != "$canonical_dest/speedrund" ] && [ "$command" != "$logical_dest/speedrund" ]; then
      command=$(tr '\000' '\n' <"/proc/$pid/cmdline" | sed -n '1p') ||
        die "could not inspect daemon arguments $pid in /proc; binaries left untouched"
    fi
  else
    command -v ps >/dev/null 2>&1 || die "needs ps to check the running daemon; binaries left untouched"
    command=$(ps -p "$pid" -o command= 2>/dev/null) ||
      die "could not inspect daemon process $pid with ps; binaries left untouched"
  fi
  [ "$command" = "$canonical_dest/speedrund" ] || [ "$command" = "$logical_dest/speedrund" ]
}

if [ "${1:-}" = "--uninstall" ]; then
  [ "$#" -eq 1 ] || die "usage: install.sh --uninstall"
  manifest="$dest/.agentspeedrun-installed"
  [ -f "$manifest" ] || die "no installation record at $manifest; binaries left untouched"
  if matching_daemon; then
    "$dest/speedrun" daemon stop || die "could not stop the installed daemon; binaries left untouched"
  fi
  for b in speedrun speedrund; do
    [ -f "$dest/$b" ] || continue
    expected=$(grep " $b\$" "$manifest" || true)
    actual=$(cd "$dest" && {
      if command -v sha256sum >/dev/null 2>&1; then sha256sum "$b"; else shasum -a 256 "$b"; fi
    })
    if [ -n "$expected" ] && [ "$actual" = "$expected" ]; then
      rm "$dest/$b"
      say "removed $dest/$b"
    else
      say "left modified $dest/$b untouched"
    fi
  done
  rm "$manifest"
  say "project data, host adapters and configuration were preserved; remove adapters with speedrun setup HOST --remove before uninstalling"
  exit 0
fi
[ "$#" -eq 0 ] || die "usage: install.sh [--uninstall]"

case "$(uname -s)/$(uname -m)" in
  MINGW*/* | MSYS*/* | CYGWIN*/*) die "Windows is not supported; install inside WSL" ;;
  Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-musl" ;;
  Linux/aarch64 | Linux/arm64) target="aarch64-unknown-linux-musl" ;;
  Darwin/arm64) target="aarch64-apple-darwin" ;;
  Darwin/x86_64) target="x86_64-apple-darwin" ;;
  *) die "no prebuilt binaries for $(uname -s) $(uname -m); build from source: git clone https://github.com/$repo && cd agentspeedrun && cargo install --locked --path crates/speedrun-cli && cargo install --locked --path crates/speedrund" ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

if [ -n "${SPEEDRUN_DOWNLOAD_URL:-}" ]; then
  [ -n "$version" ] || die "SPEEDRUN_DOWNLOAD_URL needs SPEEDRUN_VERSION"
  for f in SHA256SUMS "agentspeedrun-$version-$target.tar.gz"; do
    curl -fsSL "$SPEEDRUN_DOWNLOAD_URL/$f" -o "$tmp/$f" || die "could not download $SPEEDRUN_DOWNLOAD_URL/$f"
  done
elif command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  # Works for private repositories too. Without a tag, gh picks the latest release.
  [ -n "$version" ] || version="$(gh release view -R "$repo" --json tagName -q .tagName)" ||
    die "could not find a release of $repo"
  gh release download "$version" -R "$repo" -D "$tmp" -p "*-$target.tar.gz" -p SHA256SUMS ||
    die "could not download $version for $target"
else
  command -v curl >/dev/null 2>&1 || die "needs curl (or an authenticated gh)"
  if [ -z "$version" ]; then
    version="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")" ||
      die "could not find the latest release (private repo? install and log in to gh first)"
    version="${version##*/}"
  fi
  base="https://github.com/$repo/releases/download/$version"
  curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" ||
    die "could not download $version (private repo? install and log in to gh first)"
  curl -fsSL "$base/agentspeedrun-$version-$target.tar.gz" -o "$tmp/agentspeedrun-$version-$target.tar.gz" ||
    die "could not download $version for $target"
fi

archive="agentspeedrun-$version-$target.tar.gz"
[ -f "$tmp/$archive" ] || die "release $version has no $archive"
(
  cd "$tmp"
  grep " $archive\$" SHA256SUMS >expected || die "SHA256SUMS does not list $archive"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c expected >/dev/null
  else
    shasum -a 256 -c expected >/dev/null
  fi
) || die "checksum mismatch for $archive"

# Provenance (V0.2 W8): the release workflow attests every archive it builds.
if [ -z "${SPEEDRUN_SKIP_ATTESTATION:-}" ] && command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  gh attestation verify "$tmp/$archive" --repo "$repo" >/dev/null ||
    die "$archive has no valid build attestation from $repo"
  say "verified the build attestation of $archive"
fi

tar -xzf "$tmp/$archive" -C "$tmp"
restart=0
if matching_daemon; then restart=1; fi
for b in speedrun speedrund; do
  if [ ! -f "$tmp/agentspeedrun-$version-$target/$b" ] || [ ! -x "$tmp/agentspeedrun-$version-$target/$b" ]; then
    die "archive is missing executable $b"
  fi
done
mkdir -p "$dest"
# Stage the complete pair before replacing either executable.
for b in speedrun speedrund; do
  cp "$tmp/agentspeedrun-$version-$target/$b" "$dest/.$b.new"
  chmod 755 "$dest/.$b.new"
done
for b in speedrun speedrund; do
  # Rename so a running daemon keeps its old inode.
  mv -f "$dest/.$b.new" "$dest/$b"
done

say "installed $("$dest/speedrun" --version) to $dest"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) say "add $dest to your PATH, e.g. echo 'export PATH=\"$dest:\$PATH\"' >> ~/.profile" ;;
esac
# Record exact installed bytes so uninstall does not remove a later replacement.
(cd "$dest" && {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum speedrun speedrund; else shasum -a 256 speedrun speedrund; fi
} >.agentspeedrun-installed)
if [ "$restart" -eq 1 ]; then
  # Replace the running daemon with the new one, waiting for the old to exit.
  if "$dest/speedrun" daemon restart >/dev/null 2>&1; then
    say "restarted speedrund on the new version"
  else
    say "a speedrund is running; restart it to use the new version: speedrun daemon restart"
  fi
fi
say "next: cd into a project and run: speedrun setup"
