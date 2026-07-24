#!/usr/bin/env bash
# Build the claudo web client bundle into crates/frontends/egui/dist.
# Requires: rustup target add wasm32-unknown-unknown
#           cargo install wasm-bindgen-cli --version <matching Cargo.lock>
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=crates/frontends/egui/dist

cargo build -p claudo-egui --target wasm32-unknown-unknown --release
wasm-bindgen --target web --no-typescript \
  --out-dir "$OUT" --out-name claudo \
  target/wasm32-unknown-unknown/release/claudo.wasm
cp crates/frontends/egui/web/index.html "$OUT/index.html"

echo "web bundle -> $OUT"
echo "serve it with:  cargo run -p claudo-daemon"
