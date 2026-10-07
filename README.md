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
dut ~/ -o home.dut --no-ui         scan and save without the UI (cron-friendly)
dut ~/ --diff home.dut             scan and show what changed since the snapshot
dut -f home.dut -r -o home.dut --no-ui   refresh a snapshot headlessly
```

Press `?` in the UI for keys.

## Development

```
nix develop
cargo build --release
```
