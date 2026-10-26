#!/bin/sh
# herdr runs this as the plugin's build step, so `herdr plugin install` alone sets
# everything up: it builds the binary, then `herdr-rooms install` links it onto PATH
# and sets up Claude Code and Codex (see the README). HERDR_ROOMS_SKIP_SETUP=1 builds
# only. In a folder you cloned or unzipped yourself, `sh scripts/install.sh --link`
# also registers the folder with herdr (`herdr plugin link`), which herdr's own
# install does by itself. Without cargo it stops and says what to install.
set -eu
cd "$(dirname "$0")/.."
if ! command -v cargo >/dev/null 2>&1; then
  echo "herdr-rooms: cargo is not installed. Install Rust (https://rustup.rs), then run this again." >&2
  exit 1
fi
cargo build --locked --release
if [ "${HERDR_ROOMS_SKIP_SETUP:-}" = 1 ]; then
  echo "herdr-rooms: HERDR_ROOMS_SKIP_SETUP=1, so the agents are not set up; run: herdr plugin action invoke install --plugin herdr-rooms"
else
  ./target/release/herdr-rooms install
fi
if [ "${1:-}" = --link ]; then
  if ! command -v herdr >/dev/null 2>&1; then
    echo "herdr-rooms: herdr is not on PATH, so this folder is not linked. Install herdr, then run: herdr plugin link \"$(pwd)\"" >&2
    exit 1
  fi
  herdr plugin link "$(pwd)" >/dev/null
  echo "linked   $(pwd) as the herdr plugin herdr-rooms"
fi
