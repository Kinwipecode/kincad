@echo off
cd /d "%~dp0"
cargo run --bin dwgviewer %*
