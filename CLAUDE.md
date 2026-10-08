# dut

A TUI disk usage analyzer in Rust (ratatui). It scans in parallel, caches
every full scan as a snapshot, and shows what changed since the last one.
Runs on Linux and macOS. See README.md for user-facing behavior.

## Commands

```sh
cargo test
cargo clippy --all-targets -- -D warnings   # CI fails on any warning
cargo fmt
cargo build --release
nix build                                   # also runs the tests
```

Linux code paths don't compile on macOS and vice versa. From a Mac, check
the Linux side with
`cargo clippy --all-targets --target x86_64-unknown-linux-gnu -- -D warnings`
(needs `rustup target add x86_64-unknown-linux-gnu`). CI runs both platforms.

## Layout

- `main.rs`: CLI (clap), choosing what to show first (snapshot, cache,
  cached ancestor, fresh scan), headless `--no-ui` mode.
- `scan.rs`: the parallel scanner (rayon), excludes, progress and the
  ETA estimate, including the filesystem-usage estimate.
- `mounts.rs`: the mount table, which mounts are skipped by default
  (virtual, network, snapshots), and per-mount usage.
- `macos.rs`: `getattrlistbulk` directory reads, firmlinks, volume usage.
  Most of the `unsafe` lives here.
- `tree.rs`: arena tree (`u32` node ids) and the per-node `flags`.
- `snapshot.rs`: the on-disk format (lz4, varints, preorder, own sizes only).
- `cache.rs`: automatic snapshots: keys, history, retention.
- `diff.rs`: comparing two trees. `delete.rs`: safe recursive delete.
- `app.rs`: UI state, keys, background jobs. `ui.rs`: drawing.
  `treemap.rs`: squarified layout. `fmt.rs`: sizes, counts, times.

## Things to keep stable

- **Snapshot format.** Old snapshots must keep loading. Bump `VERSION` in
  `snapshot.rs` only for incompatible changes and keep reading older
  versions. The options byte holds bit 0 = `-x`, bit 1 = `--all-mounts`.
  Node flags are stored raw, so add new flags as new bits; never renumber.
- **Cache keys.** `cache::Key` hashes the root and the options that change
  results. A new option should only affect the hash when it's set, so
  existing users' history keeps matching.
- **Platform code** goes behind `#[cfg(target_os = ...)]`, with a fallback
  for other Unixes where it makes sense.

## Conventions

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
  `type(scope): summary`, with types like `feat`, `fix`, `perf`, `refactor`,
  `docs`, `test`, `build`, `ci` and `chore`. The scope is optional, often a
  module or platform (`feat(macos): ...`, `fix(scan): ...`). Mark breaking
  changes with `!` or a `BREAKING CHANGE:` footer.
- Comments explain why, not what. Keep them as short as the surrounding ones.
- Prefer std and libc over new dependencies.
- Scanning must never follow symlinks, and deleting must never cross into
  another filesystem.
- Changes users would notice go in README.md.
- `demo/` (VHS recording) is Linux-only and needs no macOS port.

## Releases

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests and the Nix build
on Linux and macOS, and pushes Nix outputs to the `dut` Cachix cache.
Pushing a `v*` tag runs `release.yml`, which publishes static Linux and
macOS binaries. Keep the version in `Cargo.toml` and `flake.nix` in sync
with the tag.
