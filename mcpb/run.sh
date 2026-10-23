#!/bin/sh
# MCPB launcher for macOS and Linux: pick the binary for this CPU.
dir=$(dirname "$0")
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) bin=recall-darwin-arm64 ;;
  Darwin-x86_64) bin=recall-darwin-x86_64 ;;
  Linux-x86_64 | Linux-amd64) bin=recall-linux-x86_64 ;;
  Linux-aarch64 | Linux-arm64) bin=recall-linux-aarch64 ;;
  *) echo "recall: no prebuilt binary for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
# Zip extraction may drop the executable bit.
[ -x "$dir/$bin" ] || chmod +x "$dir/$bin" 2>/dev/null
exec "$dir/$bin" "$@"
