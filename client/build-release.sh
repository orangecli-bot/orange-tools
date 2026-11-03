#!/bin/sh
# Reproducible release build of air-client: run from the crate dir (client/ in the public repo).
set -e
REG=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/ | head -1)
export RUSTFLAGS="--remap-path-prefix=$PWD=/build/air-client --remap-path-prefix=${REG%/}=/cargo/registry --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
export CARGO_INCREMENTAL=0 SOURCE_DATE_EPOCH=0
cargo build --release --locked
sha256sum target/release/air-client
