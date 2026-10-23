#!/bin/sh
# Post-install smoke test, run by CI on every platform.
set -eu
export PATH="$HOME/.local/bin:$PATH"
recall add "User is vegetarian." --date 2021-03
recall search "vegitarian food" | grep -q "User is vegetarian"
mkdir -p "$HOME/.cursor"
recall connect cursor
grep -q '"recall"' "$HOME/.cursor/mcp.json"
recall doctor
echo "smoke test passed"
