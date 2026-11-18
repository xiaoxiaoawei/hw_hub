#!/usr/bin/env bash
# User-local toolchain setup; preserves the checkout and active branch.
set -euo pipefail
cd /workspace/hw_hub
export CARGO_HOME=/workspace/.toolchains/cargo
export RUSTUP_HOME=/workspace/.toolchains/rustup
export PATH="$CARGO_HOME/bin:$PATH"
mkdir -p /workspace/shared/downloads
if ! command -v rustup >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
        https://sh.rustup.rs -o /workspace/shared/downloads/rustup-init.sh
    sh /workspace/shared/downloads/rustup-init.sh -y --profile minimal \
        --default-toolchain none --no-modify-path
fi
rustup toolchain install 1.100.0 --profile minimal --component rustfmt --component clippy \
    --target x86_64-pc-windows-gnu
cargo fetch --locked
rustc --version
cargo --version
