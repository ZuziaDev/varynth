# Varynth CLI installer for Windows PowerShell
# Usage: irm https://varynth.zuzia.dev/install.ps1 | iex
$ErrorActionPreference = "Stop"

Write-Host "==> Installing Varynth CLI (@zuziadev/varynth)..." -ForegroundColor Cyan

if (Get-Command npm -ErrorAction SilentlyContinue) {
    Write-Host "==> Detected Node.js / npm. Installing @zuziadev/varynth globally..."
    npm install -g @zuziadev/varynth
} elseif (Get-Command cargo -ErrorAction SilentlyContinue) {
    Write-Host "==> Detected Rust / Cargo. Building Varynth from source..."
    cargo install --git https://github.com/ZuziaDev/varynth.git varynth
} else {
    Write-Error "Neither 'npm' (Node.js >= 18) nor 'cargo' (Rust toolchain) was found on PATH. Install Node.js or Rust and re-run."
    exit 1
}

Write-Host "==> Varynth CLI installed. Run 'varynth --version' or 'varynth' to start." -ForegroundColor Green
