#!/bin/sh
# Knapsack one-line installer (Linux/macOS):
#   curl -fsSL https://knapsack.dev/install.sh | sh
# Downloads the release binary, verifies its checksum, installs to ~/.knapsack/bin,
# then wires the Claude Code hook + MCP server via `knapsack install --apply`.
# Override: KNAPSACK_VERSION=vX.Y.Z  KNAPSACK_REPO=owner/repo  KNAPSACK_BASE_URL=...
set -eu

REPO="${KNAPSACK_REPO:-knapsack-dev/knapsack}"
VERSION="${KNAPSACK_VERSION:-latest}"
BASE="${KNAPSACK_BASE_URL:-https://github.com/$REPO/releases}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  plat="unknown-linux-gnu" ;;
  Darwin) plat="apple-darwin" ;;
  *) echo "knapsack: unsupported OS '$os' — on Windows use install.ps1" >&2; exit 1 ;;
esac
case "$arch" in
  x86_64|amd64)  cpu="x86_64" ;;
  arm64|aarch64) cpu="aarch64" ;;
  *) echo "knapsack: unsupported arch '$arch'" >&2; exit 1 ;;
esac

target="${cpu}-${plat}"
asset="knapsack-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="$BASE/latest/download/$asset"
else
  url="$BASE/download/$VERSION/$asset"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "knapsack: downloading $url"
curl -fsSL "$url" -o "$tmp/$asset"
curl -fsSL "$url.sha256" -o "$tmp/$asset.sha256"

echo "knapsack: verifying checksum"
( cd "$tmp" && \
  if command -v sha256sum >/dev/null 2>&1; then sha256sum -c "$asset.sha256"; \
  elif command -v shasum    >/dev/null 2>&1; then shasum -a 256 -c "$asset.sha256"; \
  else echo "knapsack: no sha256 tool (install coreutils)"; exit 1; fi )

tar -xzf "$tmp/$asset" -C "$tmp"
dest="$HOME/.knapsack/bin"
mkdir -p "$dest"
install -m 0755 "$tmp/knapsack" "$dest/knapsack"
echo "knapsack: installed $dest/knapsack"

case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "knapsack: add to PATH ->  export PATH=\"\$HOME/.knapsack/bin:\$PATH\"" ;;
esac

# Wire the hook + MCP, back up config, smoke test, doctor, print rollback.
"$dest/knapsack" install --apply
