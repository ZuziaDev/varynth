#!/usr/bin/env sh
# Varynth CLI installer for Linux and macOS
# Usage: curl -fsSL https://varynth.zuzia.dev/install.sh | bash
set -eu

printf "==> Installing Varynth CLI (v0.1.3)...\n"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

TARGET_DIR="$HOME/.local/bin"
if [ ! -d "$TARGET_DIR" ]; then
  mkdir -p "$TARGET_DIR"
fi

BIN_PATH="$TARGET_DIR/varynth"

if [ "$OS" = "linux" ] && [ "$ARCH" = "x86_64" ]; then
  printf "==> Detected Linux x86_64. Downloading pre-built binary...\n"
  curl -fsSL https://varynth.zuzia.dev/downloads/varynth-linux-amd64 -o "$BIN_PATH"
  chmod +x "$BIN_PATH"
elif command -v cargo >/dev/null 2>&1; then
  printf "==> Building Varynth from crates.io via Cargo...\n"
  cargo install varynth
elif command -v npm >/dev/null 2>&1; then
  printf "==> Installing @zuziadev/varynth via npm...\n"
  npm install -g @zuziadev/varynth
else
  printf "!! Could not find pre-built binary for $OS-$ARCH.\n"
  printf "   Please install Rust (https://rustup.rs) or Node.js to build.\n"
  exit 1
fi

case ":$PATH:" in
  *":$TARGET_DIR:"*) ;;
  *)
    printf "\n==> NOTE: Add $TARGET_DIR to your PATH by adding this line to your ~/.bashrc or ~/.zshrc:\n"
    printf "    export PATH=\"\$HOME/.local/bin:\$PATH\"\n\n"
    ;;
esac

printf "==> Varynth CLI installed successfully! Run 'varynth --version' or 'varynth' to start.\n"
