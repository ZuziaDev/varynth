#!/usr/bin/env sh
# Varynth CLI installer for Linux and macOS
# Usage: curl -fsSL https://varynth.zuzia.dev/install.sh | sh
set -eu

printf "==> Installing Varynth CLI (@zuziadev/varynth)...\n"

if command -v npm >/dev/null 2>&1; then
  printf "==> Detected Node.js / npm. Installing @zuziadev/varynth globally...\n"
  npm install -g @zuziadev/varynth
elif command -v cargo >/dev/null 2>&1; then
  printf "==> Detected Rust / Cargo. Building Varynth from source...\n"
  cargo install --git https://github.com/ZuziaDev/varynth.git varynth
else
  printf "!! Neither 'npm' (Node.js >= 18) nor 'cargo' (Rust toolchain) was found on PATH.\n"
  printf "   Install Node.js (https://nodejs.org) or Rust (https://rustup.rs) and re-run.\n"
  exit 1
fi

printf "==> Varynth CLI installation complete. Run 'varynth --version' or 'varynth' to start.\n"
