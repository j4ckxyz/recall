#!/bin/sh
# Rebuild web/app.css from the Tailwind classes used in index.html and app.js.
# Uses the Tailwind standalone CLI (no Node): https://github.com/tailwindlabs/tailwindcss/releases
set -eu
cd "$(dirname "$0")"
TW="${TAILWIND:-../tools/tailwindcss}"
"$TW" -i input.css -o app.css --minify
