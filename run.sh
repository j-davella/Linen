#!/usr/bin/env bash
cd "$(dirname "$0")"

if command -v cargo &>/dev/null; then
    exec cargo run --manifest-path src-tauri/Cargo.toml "$@"
else
    exec nix-shell shell.nix --run "cargo run --manifest-path src-tauri/Cargo.toml"
fi
