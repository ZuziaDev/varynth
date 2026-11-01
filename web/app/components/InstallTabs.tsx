"use client";

import React, { useState } from "react";

type InstallMethod = {
  id: string;
  label: string;
  badge: string;
  shell: string;
  primaryCmd: string;
  secondaryCmds: { prompt: string; cmd: string; note: string }[];
  description: string;
};

const METHODS: InstallMethod[] = [
  {
    id: "windows",
    label: "Windows PowerShell",
    badge: "irm | iex",
    shell: "PowerShell 5.1 / 7+ (x86_64-pc-windows-msvc)",
    primaryCmd: "irm https://varynth.zuzia.dev/install.ps1 | iex",
    secondaryCmds: [
      {
        prompt: "PS>",
        cmd: "varynth install --startup",
        note: "Optional: register background tray & startup launcher",
      },
      {
        prompt: "PS>",
        cmd: "varynth --model varynth/gpt-oss-120b",
        note: "Launch full-screen TUI immediately with zero API key setup",
      },
    ],
    description:
      "Native Windows bootstrapper using Invoke-RestMethod (irm) piped to Invoke-Expression (iex). Automatically provisions varynth.exe into %USERPROFILE%\\.local\\bin and updates PATH.",
  },
  {
    id: "unix",
    label: "Linux & macOS",
    badge: "curl | bash",
    shell: "bash / zsh / POSIX sh (Linux & Apple Silicon / Intel macOS)",
    primaryCmd: "curl -fsSL https://varynth.zuzia.dev/install.sh | bash",
    secondaryCmds: [
      {
        prompt: "$",
        cmd: "varynth doctor",
        note: "Verify sandbox, LSP servers, MCP bridges, and gateway health",
      },
      {
        prompt: "$",
        cmd: "varynth login",
        note: "Optional: authenticate via RFC 7636 PKCE to sync Pro skills",
      },
    ],
    description:
      "Standard shell installer for Ubuntu, Debian, Arch, Fedora, and macOS. Downloads the pre-built Linux x86_64 binary directly to ~/.local/bin/varynth or builds via Cargo / npm.",
  },
  {
    id: "node",
    label: "Node.js",
    badge: "npm / pnpm / bun",
    shell: "Node.js >= 18 (@zuziadev/varynth)",
    primaryCmd: "npm i -g @zuziadev/varynth",
    secondaryCmds: [
      {
        prompt: "$",
        cmd: "pnpm add -g @zuziadev/varynth",
        note: "Alternative global install via pnpm",
      },
      {
        prompt: "$",
        cmd: "bun add -g @zuziadev/varynth",
        note: "Alternative global install via Bun runtime",
      },
    ],
    description:
      "Cross-platform package distribution on npm. Ships prebuilt Windows binaries and runs an automated Cargo release build hook on Linux and macOS.",
  },
  {
    id: "rust",
    label: "Rust / Cargo",
    badge: "cargo install",
    shell: "rustc >= 1.80 stable (source build)",
    primaryCmd: "cargo install --git https://github.com/ZuziaDev/varynth.git varynth",
    secondaryCmds: [
      {
        prompt: "$",
        cmd: "cargo build --manifest-path cli/Cargo.toml --release",
        note: "Compile from a local repository checkout with full LTO",
      },
      {
        prompt: "$",
        cmd: "./cli/target/release/varynth install",
        note: "Copy compiled binary into ~/.local/bin/varynth",
      },
    ],
    description:
      "Direct compilation from Git or local workspace source for systems engineers who want full control over target CPU flags and link-time optimization.",
  },
];

export function InstallTabs() {
  const [activeId, setActiveId] = useState<string>("windows");
  const [copied, setCopied] = useState<boolean>(false);

  const active = METHODS.find((m) => m.id === activeId) ?? METHODS[0];

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(active.primaryCmd);
      setCopied(true);
      setTimeout(() => setCopied(false), 1800);
    } catch {
      setCopied(false);
    }
  };

  return (
    <div className="install-showcase" data-slot="install-tabs" data-kind="product">
      <div className="install-tab-bar" role="tablist" aria-label="Installation platforms">
        {METHODS.map((m) => {
          const isSelected = m.id === active.id;
          return (
            <button
              key={m.id}
              type="button"
              role="tab"
              aria-selected={isSelected}
              className={`install-tab-btn ${isSelected ? "active" : ""}`}
              onClick={() => {
                setActiveId(m.id);
                setCopied(false);
              }}
            >
              <span className="install-tab-title">{m.label}</span>
              <span className="install-tab-badge">{m.badge}</span>
            </button>
          );
        })}
      </div>

      <div className="install-terminal-body" role="tabpanel">
        <div className="install-terminal-head">
          <span className="install-shell-label">{active.shell}</span>
          <button
            type="button"
            className="copy-cmd-btn"
            onClick={handleCopy}
            aria-label="Copy primary installation command"
          >
            {copied ? "Copied to clipboard" : "Copy command"}
          </button>
        </div>

        <div className="install-primary-command" onClick={handleCopy}>
          <span className="cmd-prompt">{active.id === "windows" ? "PS C:\\>" : "$"}</span>
          <code className="cmd-code">{active.primaryCmd}</code>
        </div>

        <p className="install-method-desc">{active.description}</p>

        <div className="install-secondary-list">
          {active.secondaryCmds.map((item) => (
            <div className="install-secondary-row" key={item.cmd}>
              <div className="install-secondary-cmd">
                <span className="cmd-prompt-dim">{item.prompt}</span>
                <code>{item.cmd}</code>
              </div>
              <span className="install-secondary-note">{item.note}</span>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
