# Varynth CLI installer for Windows PowerShell
# Usage: irm https://varynth.zuzia.dev/install.ps1 | iex
$ErrorActionPreference = "Stop"

Write-Host "==> Installing Varynth CLI (v0.1.3)..." -ForegroundColor Cyan

$targetDir = "$HOME\.cargo\bin"
if (-not (Test-Path $targetDir)) {
    $targetDir = "$HOME\.local\bin"
}
if (-not (Test-Path $targetDir)) {
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
}

$exePath = Join-Path $targetDir "varynth.exe"
$url = "https://varynth.zuzia.dev/downloads/varynth.exe"

Write-Host "==> Downloading pre-built Varynth binary from $url..." -ForegroundColor Cyan
Invoke-WebRequest -Uri $url -OutFile $exePath -UseBasicParsing

# Ensure targetDir is in user PATH
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$targetDir*") {
    Write-Host "==> Adding $targetDir to user PATH..."
    [Environment]::SetEnvironmentVariable("Path", "$targetDir;$userPath", "User")
    $env:Path = "$targetDir;$env:Path"
}

Write-Host "==> Varynth CLI installed successfully!" -ForegroundColor Green
Write-Host "==> Run 'varynth --version' or 'varynth' to start." -ForegroundColor Green
