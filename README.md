# dut

Fast interactive disk usage analyzer for the terminal.

- **Fast**: parallel work-stealing scanner (rayon), `fstatat` relative to the
  open directory, compact arena tree. ~6.2M files in ~4s warm on a laptop.
- **Views**: expandable tree with size bars, squarified treemap, largest
  files, and usage by file type.
- **Any root**: `dut /some/path`; zoom in/out inside the UI.
- **One filesystem**: `-x` stays on the root's filesystem.
- **Snapshots**: save the scan (`w` in the UI, or `-o`), reopen it later with
  `-f`, and refresh all of it (`R`) or one directory (`r`).
- **Progress/ETA**: estimated from the previous scan when refreshing, or from
  filesystem usage when scanning a mount root.
- **Automatic snapshots**: every full scan is cached in `~/.cache/dut/`
  (per root and options, mode 0600). Next time, `dut PATH` opens the cached
  result instantly, refreshes it in the background, and shows what changed.
  `--cached` skips the refresh, `--no-cache` turns it off, `DUT_CACHE_DIR`
  overrides the location. A subdirectory of a cached root opens from the
  parent's snapshot. A history is kept (3 latest, then daily for a week and
  weekly for 8 weeks; roots unused for 90 days are dropped; 2 GiB cap), shown
  in the History view. `--cache-list` and `--clear-cache [PATH]` manage it.
- **Changes**: compare against an older snapshot (`--diff old.dut`) or the
  state before a refresh. The tree gets a +/- column, and view `5` lists the
  biggest growers, new and removed entries.
- **Excludes**: `-e node_modules -e '*.o' -e '/home/*/.cache'` (names, or full
  paths when the pattern has a `/`). Stored in snapshots and reused on refresh.
- **Delete** with `d` (confirmation required; never follows symlinks or
  crosses filesystems). `--no-delete` disables it.
- Disk usage (allocated blocks) by default, so sparse files count correctly;
  `a` toggles apparent size. Hard links are counted once.

## Usage

```
dut [PATH]                         scan and browse (default: .)
dut -x /                           don't cross into other mounts
dut -f home.dut                    browse a saved snapshot
dut -f home.dut --refresh          rescan the snapshot's root, then browse
dut ~/ --no-ui                     refresh the automatic snapshot (cron-friendly)
dut -c ~/                          open the automatic snapshot without rescanning
dut ~/ -o home.dut --no-ui         scan and also save to a file
dut ~/ --diff home.dut             scan and show what changed since the snapshot
dut -f home.dut -r -o home.dut --no-ui   refresh a snapshot headlessly
```

Press `?` in the UI for keys.

## Development

```
nix develop
cargo build --release
```
