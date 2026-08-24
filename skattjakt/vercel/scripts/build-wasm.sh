#!/usr/bin/env bash
# Compiles the analysis engine to WebAssembly and writes the JS bindings the
# functions import.
#
# Runs in Vercel's build step. The Rust toolchain is not on a Vercel builder by
# default, so the first thing this does is install it — pinned, because a
# toolchain that drifts is a rule set that computes differently between two
# deploys of the same commit.
set -euo pipefail

RUST_VERSION="${RUST_VERSION:-1.90.0}"
BINDGEN_VERSION="0.2.127"   # must equal the wasm-bindgen crate version in Cargo.lock
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib/engine"

if ! command -v cargo >/dev/null 2>&1; then
    echo "installing rust $RUST_VERSION"
    curl -sSf https://sh.rustup.rs | sh -s -- -y \
        --profile minimal --default-toolchain "$RUST_VERSION"
    export PATH="$HOME/.cargo/bin:$PATH"
fi
rustup target add wasm32-unknown-unknown

if ! command -v wasm-bindgen >/dev/null 2>&1; then
    cargo install wasm-bindgen-cli --version "$BINDGEN_VERSION" --locked
fi

cd "$ROOT"
# `--no-default-features` is what keeps reqwest and tokio's networking out;
# neither links on wasm32. See crates/model/Cargo.toml.
cargo build -p skattjakt-wasm --release --target wasm32-unknown-unknown --no-default-features

mkdir -p "$OUT"
wasm-bindgen "target/wasm32-unknown-unknown/release/skattjakt_wasm.wasm" \
    --out-dir "$OUT" --target nodejs

# wasm-bindgen's nodejs target emits CommonJS, and the app is an ES module
# package. One file says the generated directory is not.
cat > "$OUT/package.json" <<'PKG'
{
  "//": "Generated. wasm-bindgen emits CommonJS; the app is an ES module package.",
  "type": "commonjs"
}
PKG

echo "engine built into $OUT"
ls -la "$OUT"
