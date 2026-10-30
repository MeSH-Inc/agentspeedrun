#!/bin/sh
# Install the speedrun CLI and daemon from a GitHub release of mesh-inc/agentspeedrun.
#
#   curl -fsSL https://raw.githubusercontent.com/mesh-inc/agentspeedrun/main/install.sh | sh
#
# While the repository is private, fetch it with an authenticated GitHub CLI
# instead; the script then downloads through `gh` too:
#
#   gh api repos/mesh-inc/agentspeedrun/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
#
# Environment:
#   SPEEDRUN_VERSION      release tag to install, e.g. v0.1.0 (default: latest)
#   SPEEDRUN_INSTALL_DIR  where to put the binaries (default: ~/.local/bin)
#   SPEEDRUN_DOWNLOAD_URL base URL holding the release assets, for mirrors
#                         and tests (needs SPEEDRUN_VERSION)
set -eu

repo="mesh-inc/agentspeedrun"
version="${SPEEDRUN_VERSION:-}"
dest="${SPEEDRUN_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'speedrun-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-musl" ;;
  Linux/aarch64 | Linux/arm64) target="aarch64-unknown-linux-musl" ;;
  Darwin/arm64) target="aarch64-apple-darwin" ;;
  Darwin/x86_64) target="x86_64-apple-darwin" ;;
  *) die "no prebuilt binaries for $(uname -s) $(uname -m); build from source with cargo install --path crates/speedrun-cli --path crates/speedrund" ;;
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

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$dest"
for b in speedrun speedrund; do
  # Replace by rename so a running daemon keeps its old inode.
  cp "$tmp/agentspeedrun-$version-$target/$b" "$dest/.$b.new"
  chmod 755 "$dest/.$b.new"
  mv -f "$dest/.$b.new" "$dest/$b"
done

say "installed $("$dest/speedrun" --version) to $dest"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) say "add $dest to your PATH, e.g. echo 'export PATH=\"$dest:\$PATH\"' >> ~/.profile" ;;
esac
if pgrep -x speedrund >/dev/null 2>&1; then
  say "a speedrund is running; restart it to use the new version: pkill -x speedrund (the CLI starts it again)"
fi
say "next: cd into a project and run: speedrun init"
