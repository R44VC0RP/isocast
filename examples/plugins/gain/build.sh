#!/bin/sh
# Build the example's components (gain.wasm, ducker.wasm) from Rust:
#
#   rustup target add wasm32-wasip2   # once
#   examples/plugins/gain/build.sh
#
# The built components are kept in the folder, so `isocast plugin add
# examples/plugins/gain --allow` works without building. Build output goes
# to $CARGO_TARGET_DIR, else outside the plugin's folder (which `plugin add`
# copies whole).
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
target="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/isocast-plugin-gain}"
cd "$here"
CARGO_TARGET_DIR="$target" cargo build --release --target wasm32-wasip2
for name in gain ducker; do
    cp "$target/wasm32-wasip2/release/$name.wasm" "$here/$name.wasm"
    echo "built $here/$name.wasm ($(wc -c <"$here/$name.wasm" | tr -d ' ') bytes)"
done
