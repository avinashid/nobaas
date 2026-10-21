#!/bin/sh
# Builds the browser module into packages/js/pkg/ (size-optimised, no wasm-bindgen needed).
set -e
cd "$(dirname "$0")/../.."
CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
CARGO_PROFILE_RELEASE_STRIP=true CARGO_PROFILE_RELEASE_PANIC=abort \
  cargo build --release -p peerwire-ffi --target wasm32-unknown-unknown --target-dir target/web
mkdir -p packages/js/pkg
cp target/web/wasm32-unknown-unknown/release/peerwire_ffi.wasm packages/js/pkg/
ls -l packages/js/pkg/peerwire_ffi.wasm
