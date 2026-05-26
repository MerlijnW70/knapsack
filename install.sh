#!/bin/sh
# Knapsack one-line installer (Linux/macOS):
#   curl -fsSL https://raw.githubusercontent.com/MerlijnW70/knapsack/main/install.sh | sh
#
# Downloads the release binary, verifies its SHA-256, installs to ~/.knapsack/bin,
# and wires the Claude Code hook + MCP server via `knapsack install`.
#
# Fails loud + actionable: every step has a recovery hint. Network blips retry; a
# malformed config exits with a clear error and the doctor pointer.
# Overrides: KNAPSACK_VERSION=vX.Y.Z  KNAPSACK_REPO=owner/repo  KNAPSACK_BASE_URL=...
# KNAPSACK_VERBOSE=1
set -eu

VERBOSE="${KNAPSACK_VERBOSE:-}"
say()     { printf 'knapsack: %s\n' "$1"; }
whisper() { [ -n "$VERBOSE" ] && printf '  %s\n' "$1" || true; }
die() {
  printf '\n'
  printf 'knapsack install failed:\n  %s\n' "$1" >&2
  [ -n "${2:-}" ] && printf '\n  %s\n' "$2" >&2
  printf '\n'
  exit 1
}

# --- 1. detect platform -------------------------------------------------------
REPO="${KNAPSACK_REPO:-MerlijnW70/knapsack}"
VERSION="${KNAPSACK_VERSION:-latest}"
BASE="${KNAPSACK_BASE_URL:-https://github.com/$REPO/releases}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  plat="unknown-linux-gnu"
          # WSL hint — WSL reports Linux, which is correct (Linux binaries run there),
          # but users sometimes try to integrate with a Claude Code running on the
          # Windows side. Flag it so they can re-run install.ps1 in PowerShell if
          # that's what they wanted.
          if grep -qi 'microsoft' /proc/version 2>/dev/null; then
            whisper "detected WSL — installing Linux binary. For Claude Code on the Windows side, run install.ps1 in PowerShell instead."
          fi
          ;;
  Darwin) plat="apple-darwin" ;;
  *) die "unsupported OS '$os' — Windows users should run install.ps1 in PowerShell." ;;
esac
case "$arch" in
  x86_64|amd64)  cpu="x86_64" ;;
  arm64|aarch64) cpu="aarch64" ;;
  *) die "unsupported CPU architecture '$arch'." "knapsack ships x86_64 and aarch64 only." ;;
esac

target="${cpu}-${plat}"
asset="knapsack-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="$BASE/latest/download/$asset"
else
  url="$BASE/download/$VERSION/$asset"
fi
whisper "platform: $target"

# --- 2. download with retry ---------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $asset"
# --retry 3 --retry-delay 2 handles transient 503s and Wi-Fi blips; --retry-connrefused
# also retries on a refused connection (proxy hiccups). -A sets a real UA so GitHub
# doesn't rate-limit on the default curl UA.
if ! curl -fsSL --retry 3 --retry-delay 2 --retry-connrefused -A "knapsack-installer/0.1" "$url" -o "$tmp/$asset"; then
  die "couldn't download from $url" "Check your internet connection. If you're behind a proxy, set HTTPS_PROXY and re-run."
fi
if ! curl -fsSL --retry 3 --retry-delay 2 --retry-connrefused -A "knapsack-installer/0.1" "$url.sha256" -o "$tmp/$asset.sha256"; then
  die "couldn't download checksum file from $url.sha256" "Re-run; if it persists this is an upstream release problem."
fi

# --- 3. verify checksum -------------------------------------------------------
whisper "verifying SHA-256"
checksum_ok=0
(
  cd "$tmp"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$asset.sha256" >/dev/null 2>&1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$asset.sha256" >/dev/null 2>&1
  else
    exit 127
  fi
) && checksum_ok=1 || checksum_ok=$?
if [ "$checksum_ok" = "127" ]; then
  die "no SHA-256 tool found (need sha256sum or shasum)." "On Linux: install coreutils. On macOS: shasum ships with the OS — your PATH may be broken."
fi
if [ "$checksum_ok" != "1" ]; then
  expected=$(awk '{print $1}' "$tmp/$asset.sha256")
  die "checksum mismatch — the download is corrupt or has been tampered with" "Expected: $expected. Re-run the installer to retry."
fi

# --- 4. unpack and install ----------------------------------------------------
tar -xzf "$tmp/$asset" -C "$tmp"
dest="$HOME/.knapsack/bin"
mkdir -p "$dest"
# Linux/macOS allows overwriting a running ELF/Mach-O binary — the inode stays
# valid until the process exits, so no rename dance needed.
install -m 0755 "$tmp/knapsack" "$dest/knapsack"
whisper "installed $dest/knapsack"

# --- 5. silent PATH guidance --------------------------------------------------
# The hook + MCP use absolute paths so PATH isn't required for the product to
# work — only for typing `knapsack` in a shell. Mention it once when it's missing,
# and only in verbose mode otherwise.
case ":$PATH:" in
  *":$dest:"*) ;;
  *) whisper "to use ``knapsack`` from a shell, add it to PATH:  export PATH=\"\$HOME/.knapsack/bin:\$PATH\"" ;;
esac

# --- 6. wire hook + MCP into Claude Code -------------------------------------
# `knapsack install` now exits non-zero if any patch failed, so a malformed
# settings.json (BOM, trailing commas, wrong encoding) propagates out as a real
# script failure instead of silently leaving Claude Code unwired.
if ! "$dest/knapsack" install; then
  die "knapsack downloaded ok but couldn't wire into Claude Code (see message above)" "Run ``$dest/knapsack doctor`` to see exactly what failed."
fi

printf '\n'
printf 'Knapsack installed. Restart Claude Code to load it.\n'
printf '\n'
