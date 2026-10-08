#!/usr/bin/env bash
# Builds a fake home directory for the README demo, plus a snapshot history
# (14, 6 and 1 days old) so the Changes and History views have something to
# show. Files are allocated with fallocate: real disk usage, written instantly.
#
#   demo/make-demo.sh [DIR] [CACHE_DIR]
#
# Needs libfaketime (nix: pkgs.libfaketime) to backdate the snapshots.
set -euo pipefail

DIR=${1:-$HOME/demo}
export DUT_CACHE_DIR=${2:-$DIR-cache}
DUT=$(readlink -f "${DUT:-$(dirname "$0")/../target/release/dut}")
FAKETIME_LIB=${FAKETIME_LIB:-$(dirname "$(readlink -f "$(command -v faketime)")")/../lib/libfaketime.so.1}

rm -rf "$DIR" "$DUT_CACHE_DIR"
mkdir -p "$DIR"
cd "$DIR"

f() { mkdir -p "$(dirname "$1")"; fallocate -l "$2" "$1"; }
many() { # many DIR COUNT MAXKB EXT
    mkdir -p "$1"
    for i in $(seq 1 "$2"); do
        fallocate -l $(( (RANDOM % $3 + 1) * 1024 )) "$1/f$i.$4"
    done
}

# --- two weeks ago ---------------------------------------------------------
f Videos/holiday-2025.mkv 1400M
f Videos/talk-recording.mp4 620M
f Downloads/ubuntu-26.04-desktop-amd64.iso 2800M
f Downloads/dataset.tar.zst 410M
f Downloads/invoice-0193.pdf 180K
for y in 2024 2025; do
    for m in 01 03 05 07 09 11; do many "Photos/$y/$y-$m" 40 6000 jpg; done
done
many Music/albums/ambient 24 9000 flac
many Music/albums/jazz 31 9000 flac
many Projects/webapp/node_modules/react 120 60 js
many Projects/webapp/node_modules/typescript/lib 90 400 js
many Projects/webapp/node_modules/@babel/core 200 40 js
many Projects/webapp/node_modules/esbuild/bin 2 9000 bin
many Projects/webapp/src 60 20 ts
many Projects/rust-game/src 25 30 rs
f Projects/rust-game/target/debug/rust-game 180M
many Projects/rust-game/target/debug/deps 300 3000 rlib
many Projects/rust-game/assets/textures 80 4000 png
many Projects/notes 12 8 md
many .cache/pip 150 900 whl
many .cache/thumbnails 400 30 png
many .local/share/Steam/steamapps 6 400000 pak
mkdir -p Documents && many Documents/taxes 14 900 pdf && many Documents/letters 20 80 docx
LD_PRELOAD=$FAKETIME_LIB FAKETIME="-14d" "$DUT" -x "$DIR" --no-ui >/dev/null

# --- six days ago: more photos, a new project, cache growth ------------------
many Photos/2026/2026-01 45 6000 jpg
many Photos/2026/2026-03 38 6000 jpg
many Projects/ml-experiments/checkpoints 6 300000 pt
many .cache/pip 240 900 whl
rm -f Downloads/dataset.tar.zst
LD_PRELOAD=$FAKETIME_LIB FAKETIME="-6d" "$DUT" -x "$DIR" --no-ui >/dev/null

# --- yesterday: checkpoints keep piling up -----------------------------------
many Projects/ml-experiments/checkpoints2 8 300000 pt
f Projects/rust-game/target/release/rust-game 95M
LD_PRELOAD=$FAKETIME_LIB FAKETIME="-1d" "$DUT" -x "$DIR" --no-ui >/dev/null

# --- today (not snapshotted; the live scan finds these) ----------------------
f Videos/screen-capture-0412.mov 1900M
many Projects/ml-experiments/checkpoints3 4 300000 pt
f Projects/rust-game/target/debug/incremental/big.bin 340M
rm -rf Videos/talk-recording.mp4 Music/albums/jazz

du -sh "$DIR"
