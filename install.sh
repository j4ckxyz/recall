#!/bin/sh
# Recall installer for macOS and Linux.
#
#   curl -fsSL https://github.com/j4ckxyz/recall/releases/latest/download/install.sh | sh
#
# Environment:
#   RECALL_VERSION       release tag to install (default: latest)
#   RECALL_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   RECALL_CONNECT       yes | no — add Recall to detected agents without asking
#   RECALL_ARTIFACT_DIR  install from local release files instead of downloading (CI)
set -eu

REPO="j4ckxyz/recall"
VERSION="${RECALL_VERSION:-latest}"
INSTALL_DIR="${RECALL_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ---- platform ---------------------------------------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux) os_part="unknown-linux-musl" ;;
  Darwin) os_part="apple-darwin" ;;
  *) die "unsupported OS '$os'. On Windows run: irm https://github.com/$REPO/releases/latest/download/install.ps1 | iex" ;;
esac
case "$arch" in
  x86_64 | amd64) arch_part="x86_64" ;;
  aarch64 | arm64) arch_part="aarch64" ;;
  *) die "unsupported CPU '$arch'. Prebuilt binaries cover x86_64 and arm64 (64-bit Raspberry Pi OS). Build from source: cargo install --git https://github.com/$REPO --features vendored-openssl" ;;
esac
# Rosetta: prefer the native arm64 build on Apple silicon.
if [ "$os" = Darwin ] && [ "$arch_part" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
  arch_part="aarch64"
fi
target="${arch_part}-${os_part}"
asset="recall-${target}.tar.gz"

# ---- tools ------------------------------------------------------------------
fetch() { # url dest
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "need curl or wget"
  fi
}
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | sed 's/.*= //'
  else die "need sha256sum, shasum or openssl to verify the download"
  fi
}
command -v tar >/dev/null 2>&1 || die "need tar"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t recall)
trap 'rm -rf "$tmp"' EXIT INT TERM

# ---- download + verify -------------------------------------------------------
step "Downloading Recall ($VERSION) for $target"
if [ -n "${RECALL_ARTIFACT_DIR:-}" ]; then
  cp "$RECALL_ARTIFACT_DIR/$asset" "$RECALL_ARTIFACT_DIR/SHA256SUMS" "$tmp/" || die "missing $asset in $RECALL_ARTIFACT_DIR"
else
  if [ "$VERSION" = latest ]; then base="https://github.com/$REPO/releases/latest/download"
  else base="https://github.com/$REPO/releases/download/$VERSION"; fi
  fetch "$base/$asset" "$tmp/$asset" || die "download failed: $base/$asset"
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
fi

expected=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
[ -n "$expected" ] || die "$asset is not listed in SHA256SUMS"
actual=$(sha256 "$tmp/$asset")
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)"
say "Checksum verified (sha256 $actual)"

# ---- install ----------------------------------------------------------------
step "Installing to $INSTALL_DIR"
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
# Replace atomically so a running `recall mcp` keeps its old inode.
cp "$tmp/recall" "$INSTALL_DIR/.recall.new"
chmod 755 "$INSTALL_DIR/.recall.new"
mv -f "$INSTALL_DIR/.recall.new" "$INSTALL_DIR/recall"
bin="$INSTALL_DIR/recall"
"$bin" --version || die "the installed binary does not run on this system"

# ---- set up + verify --------------------------------------------------------
step "Creating your encrypted memory store"
"$bin" init

step "Self-test"
"$bin" doctor || die "self-test failed; please open an issue: https://github.com/$REPO/issues"

# ---- connect agents ---------------------------------------------------------
connect="${RECALL_CONNECT:-ask}"
if [ "$connect" = ask ]; then
  if [ -r /dev/tty ] && [ -w /dev/tty ] && (: </dev/tty) 2>/dev/null; then
    printf '\nAdd Recall to the agents found above? [Y/n] ' >/dev/tty
    read -r reply </dev/tty || reply=n
    case "$reply" in [nN]*) connect=no ;; *) connect=yes ;; esac
  else
    connect=no
  fi
fi
if [ "$connect" = yes ]; then
  step "Connecting agents"
  "$bin" connect || say "Some agents could not be configured; see above."
fi

# ---- done -------------------------------------------------------------------
step "Recall is installed"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    say "Add $INSTALL_DIR to your PATH:"
    case "$(basename "${SHELL:-sh}")" in
      fish) say "  fish_add_path $INSTALL_DIR" ;;
      zsh) say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc" ;;
      *) say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc" ;;
    esac
    ;;
esac
say "Key: ~/.recall/key — back it up; without it your memory cannot be decrypted."
[ "$connect" = yes ] || say "Connect your agents any time with: recall connect"
say "Share one memory across devices with Tailscale: https://github.com/$REPO#use-it-across-devices-with-tailscale"
