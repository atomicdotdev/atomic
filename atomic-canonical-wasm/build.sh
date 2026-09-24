#!/bin/sh
# Build the browser module: pkg/atomic_canonical_wasm{.js,_bg.wasm}.
# Needs the wasm32 target and a wasm-bindgen CLI matching the locked
# wasm-bindgen crate:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version <locked version>
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cargo build -p atomic-canonical-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir "${1:-$here/pkg}" \
    "$here/../target/wasm32-unknown-unknown/release/atomic_canonical_wasm.wasm"
