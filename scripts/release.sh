#!/usr/bin/env bash
# Cuts a release: bumps the version, checks the build, commits, tags, and
# (after asking) pushes. CI's release workflow takes it from there.
#
#   scripts/release.sh 0.2.0
set -euo pipefail

die() {
  echo "release: $*" >&2
  exit 1
}

[ $# -eq 1 ] || die "usage: $0 <version>   (e.g. 0.2.0)"
version=${1#v}
tag=v$version
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
  die "$1 isn't a version like 1.2.3 or 1.2.3-rc.1"

cd "$(git rev-parse --show-toplevel)"

[ "$(git branch --show-current)" = main ] || die "not on main"
[ -z "$(git status --porcelain)" ] || die "working tree isn't clean"
git fetch --quiet --tags origin
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
  die "main isn't in sync with origin/main (pull or push first)"
! git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null || die "$tag already exists"

current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
[ "$current" != "$version" ] || die "Cargo.toml is already at $version"

# Undo the bump if anything fails before the commit.
restore() { git checkout --quiet -- Cargo.toml Cargo.lock; }
trap restore ERR

echo "==> $current -> $version"
# Only the first `version =` line, which is the package's.
awk -v v="$version" '!done && /^version = / { print "version = \"" v "\""; done = 1; next } 1' \
  Cargo.toml >Cargo.toml.new
mv Cargo.toml.new Cargo.toml
cargo build --quiet # updates Cargo.lock

echo "==> checking"
cargo fmt --check
cargo clippy --quiet --all-targets --locked -- -D warnings
cargo test --quiet --locked

git commit --quiet -m "chore: release $tag" Cargo.toml Cargo.lock
trap - ERR
git tag -a "$tag" -m "$tag"

echo
git log --oneline -1 "$tag"
read -r -p "Push main and $tag to origin? [y/N] " answer
if [ "$answer" = y ] || [ "$answer" = Y ]; then
  git push --atomic origin main "$tag"
  url=$(gh browse --no-browser 2>/dev/null || true)
  echo "==> pushed; the release workflow is building it${url:+: $url/actions}"
else
  echo "Not pushed. To push later:   git push --atomic origin main $tag"
  echo "To undo:                     git tag -d $tag && git reset --hard HEAD~1"
fi
