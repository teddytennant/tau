#!/bin/sh
# curl -fsSL https://raw.githubusercontent.com/teddytennant/tau/main/install.sh | sh
#
# TAU_VERSION=v0.1.0 picks a release, TAU_INSTALL_DIR changes where it goes.
set -eu

repo=teddytennant/tau
dir=${TAU_INSTALL_DIR:-$HOME/.local/bin}
version=${TAU_VERSION:-latest}

die() { echo "tau: $*" >&2; exit 1; }

case $(uname -s) in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) die "no build for $(uname -s); build from source with cargo install --git https://github.com/$repo" ;;
esac
case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "no build for $(uname -m)" ;;
esac
target=$arch-$os

if [ "$version" = latest ]; then
  base=https://github.com/$repo/releases/latest/download
else
  base=https://github.com/$repo/releases/download/$version
fi

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    die "need curl or wget"
  fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch "$base/tau-$target.tar.gz" "$tmp/tau.tar.gz" || die "download failed: $base/tau-$target.tar.gz"
fetch "$base/sha256sums.txt" "$tmp/sums" || die "download failed: $base/sha256sums.txt"

want=$(grep " tau-$target.tar.gz\$" "$tmp/sums" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/tau.tar.gz" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/tau.tar.gz" | cut -d' ' -f1)
fi
[ -n "$want" ] || die "no checksum for tau-$target.tar.gz"
[ "$want" = "$got" ] || die "checksum mismatch for tau-$target.tar.gz"

tar -xzf "$tmp/tau.tar.gz" -C "$tmp"
mkdir -p "$dir"
mv "$tmp/tau" "$dir/tau.new"
chmod 755 "$dir/tau.new"
mv "$dir/tau.new" "$dir/tau"

echo "installed $("$dir/tau" --version) to $dir/tau"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "$dir is not on your PATH; add it, then run tau" ;;
esac
