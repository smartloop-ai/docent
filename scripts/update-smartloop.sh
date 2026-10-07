#!/usr/bin/env bash
# Moves the smartloop crate to the newest release, or to VERSION if given,
# and re-syncs Cargo.lock with the registry when a version has been
# republished under the same number (Cargo otherwise keeps the old crate and
# fails fresh builds with "checksum ... changed between lock files").
#
#   scripts/update-smartloop.sh          # newest compatible release
#   scripts/update-smartloop.sh 1.3.0    # exactly this one
set -euo pipefail

INDEX=https://dl.smartloop.ai/crates/sm/ar/smartloop
cd "$(dirname "$0")/.."

locked() { grep -A1 '^name = "smartloop"$' Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p'; }
cksum() { curl -fsSL "$INDEX" | jq -r --arg v "$1" 'select(.vers == $v) | .cksum'; }

# A republished crate: point the lock entry at the registry's checksum, and
# drop this machine's copies of the old one (crate, sources, index entry).
sync() {
  local version=$1 want
  want=$(cksum "$version")
  [ -n "$want" ] || { echo "smartloop $version is not in the registry" >&2; exit 1; }
  if ! grep -q "checksum = \"$want\"" Cargo.lock; then
    echo "smartloop $version was republished; checksum now $want"
    V="$version" C="$want" perl -0pi -e \
      's/(name = "smartloop"\nversion = "\Q$ENV{V}\E"\nsource = "[^"]*"\nchecksum = ")[0-9a-f]+/$1$ENV{C}/' Cargo.lock
    local registry="${CARGO_HOME:-$HOME/.cargo}/registry"
    rm -f "$registry"/cache/dl.smartloop.ai-*/smartloop-"$version".crate
    rm -rf "$registry"/src/dl.smartloop.ai-*/smartloop-"$version"
    rm -f "$registry"/index/dl.smartloop.ai-*/.cache/sm/ar/smartloop
    # Cargo keys a registry crate's build on its version alone, so without
    # this it would link the old build of the same version.
    cargo clean -p smartloop --quiet
  fi
}

sync "$(locked)"
if [ -n "${1:-}" ]; then
  sed -i.bak "s/^smartloop = { version = \"[^\"]*\"/smartloop = { version = \"$1\"/" Cargo.toml && rm Cargo.toml.bak
  cargo update -p smartloop --precise "$1"
else
  cargo update -p smartloop
fi
sync "$(locked)"
# Downloads the crate and checks it against the lock.
cargo fetch --locked
echo "smartloop $(locked)"
