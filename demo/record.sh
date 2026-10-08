#!/usr/bin/env bash
# Rebuilds the demo directory and its snapshot history, then records the
# README GIF and screenshots. Recording saves a fresh snapshot, so the state
# is rebuilt every time.
#   nix shell nixpkgs#vhs nixpkgs#libfaketime -c demo/record.sh
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
demo/make-demo.sh "$HOME/demo" "$HOME/demo-cache"
vhs demo/demo.tape
rm -rf "$HOME/demo" "$HOME/demo-cache"
