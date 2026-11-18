#!/usr/bin/env bash
# Build and run Linen. On NixOS, runs inside shell.nix for the webkit/gtk libs.
cd "$(dirname "$0")"

if [ -e /etc/NIXOS ] && [ -z "$IN_NIX_SHELL" ]; then
    exec nix-shell shell.nix --run "cargo run --release --manifest-path src-tauri/Cargo.toml"
fi
exec cargo run --release --manifest-path src-tauri/Cargo.toml "$@"
