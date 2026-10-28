# Varynth

> **Autonomous AI Developer Platform & Local Coding Swarm**

[![Crates.io](https://img.shields.io/crates/v/varynth.svg)](https://crates.io/crates/varynth)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Varynth connects autonomous coding swarms, real-time terminal telemetries, and an encrypted BYOK (Bring Your Own Key) vault with zero friction.

---

## Architecture

This repository is organized as a monorepo consisting of:

- **`cli/`**: The core local Rust CLI and interactive TUI. Features 128-round autonomous tool loops, multi-model swarm coordination (Gemini, Claude, GPT, DeepSeek, Groq), local AES-256 encrypted hardware vault, OAuth PKCE authentication, and terminal session sync.
- **`api/`**: High-performance Axum cloud gateway. Handles OAuth flows (Google, GitHub), API key management, quota tracking, and model routing.
- **`web/`**: Next.js 15 production console and web dashboard live at [varynth.zuzia.dev](https://varynth.zuzia.dev).

---

## Installation

### Cargo (Recommended)

```bash
cargo install varynth
```

### Quick Shell Install

**Linux / macOS:**
```bash
curl -fsSL https://varynth.zuzia.dev/install.sh | bash
```

**Windows PowerShell:**
```powershell
irm https://varynth.zuzia.dev/install.ps1 | iex
```

---

## Quickstart

```bash
# Authenticate with Varynth Cloud
varynth auth

# Or login via browser OAuth
varynth login

# Check system health, tools, and connected models
varynth doctor

# Launch the interactive autonomous coding TUI
varynth
```

---

## License

MIT © [ZuziaDev](https://github.com/ZuziaDev)
