@echo off
rem Build (first time takes a few minutes) and run Linen on Windows.
cd /d "%~dp0"
cargo run --release --manifest-path src-tauri\Cargo.toml %*
