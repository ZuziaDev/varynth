import { InstallTabs } from "../components/InstallTabs";

export const dynamic = "force-dynamic";

export default function DocsPage() {
  return (
    <main className="page">
      <header className="page-head">
        <h1>
          Documentation and <span className="serif-accent">installation.</span>
        </h1>
        <p className="claim">
          Complete reference for installing Varynth across Windows PowerShell, Linux, macOS,
          Node.js, and Rust, plus full CLI commands, multi-agent swarm orchestration, personal
          vk_live_ API keys, cloud session sync, and gateway operations.
        </p>
      </header>

      <section className="section" aria-labelledby="install-interactive">
        <div className="section-head">
          <h2 id="install-interactive">Quick platform installer</h2>
          <p className="meta">Select your OS or package manager and click to copy</p>
        </div>
        <InstallTabs />
      </section>

      <section className="section" aria-labelledby="install-methods">
        <div className="section-head">
          <h2 id="install-methods">1. Installation methods</h2>
          <p className="meta">Windows PowerShell (irm), Linux/macOS (curl), Node.js, and Rust</p>
        </div>

        <div className="bento-grid">
          <article className="bento-card">
            <div>
              <div className="skill-top">
                <h3 className="skill-title">Windows (PowerShell irm)</h3>
                <span className="tag tag-free">windows</span>
              </div>
              <p style={{ marginTop: "var(--space-xs)" }}>
                One-line PowerShell bootstrapper using <code>Invoke-RestMethod</code> (
                <code>irm</code>) piped to <code>Invoke-Expression</code> (<code>iex</code>).
                Detects Node.js or Cargo automatically and installs the native{" "}
                <code>varynth.exe</code> binary.
              </p>
            </div>
            <div className="instrument-panel">
              <div className="instrument-head">
                <span>PowerShell 5.1 / 7+</span>
                <span className="instrument-status">x86_64-pc-windows-msvc</span>
              </div>
              <div className="instrument-body">
                <div className="term-line">
                  <span className="term-cool">PS C:\&gt;</span>
                  <span className="term-cmd">irm https://varynth.zuzia.dev/install.ps1 | iex</span>
                </div>
                <div className="term-line">
                  <span className="term-accent">persist</span>
                  <span className="term-dim">varynth install --startup</span>
                </div>
                <div className="term-line">
                  <span className="term-accent">verify</span>
                  <span className="term-ok">varynth --version</span>
                </div>
              </div>
            </div>
          </article>

          <article className="bento-card">
            <div>
              <div className="skill-top">
                <h3 className="skill-title">Linux and macOS (curl | sh)</h3>
                <span className="tag tag-free">posix</span>
              </div>
              <p style={{ marginTop: "var(--space-xs)" }}>
                Standard POSIX shell bootstrapper for Ubuntu, Debian, Arch, Fedora, and macOS
                (Apple Silicon &amp; Intel). Installs via global npm if available or builds a
                release binary with Cargo.
              </p>
            </div>
            <div className="instrument-panel">
              <div className="instrument-head">
                <span>bash / zsh / sh</span>
                <span className="instrument-status">linux &amp; darwin</span>
              </div>
              <div className="instrument-body">
                <div className="term-line">
                  <span className="term-cool">$</span>
                  <span className="term-cmd">
                    curl -fsSL https://varynth.zuzia.dev/install.sh | sh
                  </span>
                </div>
                <div className="term-line">
                  <span className="term-accent">binary</span>
                  <span className="term-dim">varynth install (~/.local/bin/varynth)</span>
                </div>
                <div className="term-line">
                  <span className="term-accent">verify</span>
                  <span className="term-ok">varynth doctor</span>
                </div>
              </div>
            </div>
          </article>

          <article className="bento-card">
            <div>
              <div className="skill-top">
                <h3 className="skill-title">Node.js (npm / pnpm / bun)</h3>
                <span className="tag tag-pro">recommended</span>
              </div>
              <p style={{ marginTop: "var(--space-xs)" }}>
                Cross-platform distribution on npm (<code>@zuziadev/varynth</code>). Ships the
                pre-compiled Windows binary directly and triggers an automated{" "}
                <code>cargo build --release</code> postinstall hook on Linux and macOS.
              </p>
            </div>
            <div className="instrument-panel">
              <div className="instrument-head">
                <span>Node.js &gt;= 18</span>
                <span className="instrument-status">@zuziadev/varynth</span>
              </div>
              <div className="instrument-body">
                <div className="term-line">
                  <span className="term-cool">npm</span>
                  <span className="term-cmd">npm i -g @zuziadev/varynth</span>
                </div>
                <div className="term-line">
                  <span className="term-cool">pnpm</span>
                  <span className="term-cmd">pnpm add -g @zuziadev/varynth</span>
                </div>
                <div className="term-line">
                  <span className="term-cool">bun</span>
                  <span className="term-cmd">bun add -g @zuziadev/varynth</span>
                </div>
              </div>
            </div>
          </article>

          <article className="bento-card span-2">
            <div>
              <div className="skill-top">
                <h3 className="skill-title">Rust Toolchain (Cargo &amp; Source Build)</h3>
                <span className="tag tag-free">cargo</span>
              </div>
              <p style={{ marginTop: "var(--space-xs)" }}>
                Build the native Rust CLI directly from the repository with full LTO optimizations
                and copy the binary to <code>~/.local/bin/varynth</code> (or{" "}
                <code>%USERPROFILE%\.local\bin\varynth.exe</code> on Windows).
              </p>
            </div>
            <div className="instrument-panel">
              <div className="instrument-head">
                <span>rustc &gt;= 1.80 (stable)</span>
                <span className="instrument-status">cli/Cargo.toml</span>
              </div>
              <div className="instrument-body">
                <div className="term-line">
                  <span className="term-cool">git + cargo</span>
                  <span className="term-cmd">
                    cargo install --git https://github.com/ZuziaDev/varynth.git varynth
                  </span>
                </div>
                <div className="term-line">
                  <span className="term-cool">local build</span>
                  <span className="term-cmd">
                    cargo build --manifest-path cli/Cargo.toml --release
                  </span>
                </div>
                <div className="term-line">
                  <span className="term-accent">self-install</span>
                  <span className="term-ok">
                    ./cli/target/release/varynth install
                  </span>
                </div>
              </div>
            </div>
          </article>

          <article className="bento-card">
            <div>
              <div className="skill-top">
                <h3 className="skill-title">System requirements</h3>
                <span className="tag">specs</span>
              </div>
              <p style={{ marginTop: "var(--space-xs)" }}>
                Varynth runs natively without container overhead unless{" "}
                <code>--sandbox docker-isolated</code> is explicitly selected.
              </p>
            </div>
            <ul className="features" style={{ margin: 0 }}>
              <li>OS: Windows 10/11, Ubuntu/Debian/Arch/Fedora, macOS 13+</li>
              <li>Node.js 18+ (for npm wrapper &amp; MCP stdio servers)</li>
              <li>Rust 1.80+ (only when building from source on Linux/macOS)</li>
              <li>Optional: Playwright + FFmpeg for 60fps motion-frames studio</li>
            </ul>
          </article>
        </div>
      </section>

      <section className="section" aria-labelledby="quickstart-flow">
        <div className="section-head">
          <h2 id="quickstart-flow">2. First launch &amp; authentication</h2>
          <p className="meta">Zero-auth public models vs. Loopback PKCE account login</p>
        </div>

        <div className="dash-grid">
          <div className="panel">
            <p className="panel-title">Immediate launch (no account required)</p>
            <p className="meta">
              Running <code>varynth</code> in any project directory starts the full-screen Terminal
              UI connected to 21 verified Varynth public models (<code>varynth/gpt-oss-120b</code>,{" "}
              <code>varynth/qwen3.8-27b</code>, <code>varynth/nemotron-3-120b-a12b</code>,{" "}
              <code>varynth/deepseek-r1-distill-qwen-32b</code>, and more).
            </p>
            <div className="code-snippet">$ cd ~/your-project &amp;&amp; varynth</div>
            <div className="code-snippet">
              $ varynth exec &quot;explain the architecture of this repo&quot;
            </div>
          </div>

          <div className="panel">
            <p className="panel-title">Account login &amp; tiered skills sync</p>
            <p className="meta">
              Sign in with <code>varynth login</code> to complete an RFC 7636 OAuth 2.0 PKCE S256
              handshake. This unlocks higher daily quotas, web search, Flux image synthesis, and
              automatic sync of Free and Pro skills from <code>/v1/catalog/skills</code>.
            </p>
            <div className="code-snippet">$ varynth login</div>
            <div className="code-snippet">$ varynth doctor</div>
          </div>
        </div>
      </section>

      <section className="section" aria-labelledby="cli-reference">
        <div className="section-head">
          <h2 id="cli-reference">3. CLI subcommands reference</h2>
          <p className="meta">Every subcommand built into the varynth binary</p>
        </div>

        <ul className="rows">
          <li className="row">
            <span className="row-id">varynth [repl]</span>
            <span className="row-name">
              Launch the interactive full-screen Terminal UI (or --plain for readline mode).
            </span>
            <span className="tag tag-free">core</span>
          </li>
          <li className="row">
            <span className="row-id">varynth exec &quot;&lt;prompt&gt;&quot;</span>
            <span className="row-name">
              Non-interactive one-shot execution (--output text | json | stream-json).
            </span>
            <span className="tag tag-free">automation</span>
          </li>
          <li className="row">
            <span className="row-id">varynth swarm &quot;&lt;task&gt;&quot;</span>
            <span className="row-name">
              Orchestrate 3 specialized cognitive agents (Architect, Builder, Reviewer) in sequence.
            </span>
            <span className="tag tag-pro">swarm</span>
          </li>
          <li className="row">
            <span className="row-id">varynth review [--staged]</span>
            <span className="row-name">
              Run an automated security, concurrency, and architecture audit on Git diffs.
            </span>
            <span className="tag tag-pro">audit</span>
          </li>
          <li className="row">
            <span className="row-id">varynth sync</span>
            <span className="row-name">
              Synchronize local .varynth/sessions/ transcripts to your cloud dashboard console.
            </span>
            <span className="tag tag-pro">cloud</span>
          </li>
          <li className="row">
            <span className="row-id">varynth resume [&lt;session-id&gt;]</span>
            <span className="row-name">
              Resume the latest session or match any full/prefix session UUID after Ctrl+C exit.
            </span>
            <span className="tag tag-free">session</span>
          </li>
          <li className="row">
            <span className="row-id">varynth sessions [fork | export]</span>
            <span className="row-name">
              List saved sessions, branch a session at message N, or export redacted Markdown/JSON.
            </span>
            <span className="tag tag-free">session</span>
          </li>
          <li className="row">
            <span className="row-id">varynth login / logout</span>
            <span className="row-name">
              Authenticate via browser PKCE flow and sync Free/Pro skills to ~/.varynth/skills/.
            </span>
            <span className="tag tag-pro">auth</span>
          </li>
          <li className="row">
            <span className="row-id">varynth endpoint [add | list | remove]</span>
            <span className="row-name">
              Attach an OpenAI- or Anthropic-compatible custom endpoint; keys stay in OS keyring.
            </span>
            <span className="tag tag-free">byok</span>
          </li>
          <li className="row">
            <span className="row-id">varynth account [connect | disconnect]</span>
            <span className="row-name">
              Connect official provider accounts (OpenAI, Anthropic, Google, Cloudflare Workers AI).
            </span>
            <span className="tag tag-free">byok</span>
          </li>
          <li className="row">
            <span className="row-id">varynth skill [list | search | install]</span>
            <span className="row-name">
              Manage local skills in .varynth/skills/ and install community skills from skills.sh.
            </span>
            <span className="tag tag-free">skills</span>
          </li>
          <li className="row">
            <span className="row-id">varynth plugin [list | install | remove]</span>
            <span className="row-name">
              Provision curated MCP plugins (github, gitlab, slack, notion, memory, git, sentry).
            </span>
            <span className="tag tag-free">mcp</span>
          </li>
          <li className="row">
            <span className="row-id">varynth mcp [status | probe | ping | serve]</span>
            <span className="row-name">
              Inspect configured MCP servers, probe tool schemas, or run the stdio MCP bridge.
            </span>
            <span className="tag tag-free">mcp</span>
          </li>
          <li className="row">
            <span className="row-id">varynth serve [--host --port]</span>
            <span className="row-name">
              Start the local operator web deck (port 7420) and OpenClaw Telegram gateway poller.
            </span>
            <span className="tag tag-pro">gateway</span>
          </li>
          <li className="row">
            <span className="row-id">varynth computer [status | doctor | enable]</span>
            <span className="row-name">
              Configure desktop control with DPI-aware coordinate overlay and Esc kill-switch.
            </span>
            <span className="tag tag-pro">operator</span>
          </li>
          <li className="row">
            <span className="row-id">varynth doctor [--fix]</span>
            <span className="row-name">
              Run comprehensive health checks across sandbox enforcement, auth, proxy, and MCP.
            </span>
            <span className="tag">diagnostics</span>
          </li>
        </ul>
      </section>

      <section className="section" aria-labelledby="tui-guide">
        <div className="section-head">
          <h2 id="tui-guide">4. Interactive TUI slash commands &amp; role routing</h2>
          <p className="meta">oh-my-pi split /model selector and 7 specialized cognitive roles</p>
        </div>

        <div className="bento-grid">
          <article className="bento-card span-2">
            <div>
              <h3>Split-pane /model browser &amp; 7 role bindings</h3>
              <p>
                Typing <code>/model</code> inside the TUI opens a two-pane browser with provider
                filtering on the left, capability tabs (<code>all</code>, <code>chat</code>,{" "}
                <code>tiny</code>, <code>image</code>, <code>tts</code>, <code>stt</code>,{" "}
                <code>judge</code>, <code>rerank</code>) on the right, and instant keys (
                <code>1-7</code> or <code>d/p/s/a/w/m/f</code>) to bind distinct models to each
                cognitive role:
              </p>
              <ul className="features">
                <li>
                  <code>default</code> (key <code>1</code> / <code>d</code>): Primary coding and
                  tool-calling driver.
                </li>
                <li>
                  <code>plan</code> (key <code>2</code> / <code>p</code>): High-reasoning architect
                  used in plan mode and <code>/plan</code>.
                </li>
                <li>
                  <code>smol</code> (key <code>3</code> / <code>s</code>): Fast model for{" "}
                  <code>/compact</code> context summarization and auto-titling.
                </li>
                <li>
                  <code>advisor</code> (key <code>4</code> / <code>a</code>): Verification auditor
                  for <code>/review</code> and <code>/goal</code> completion checks.
                </li>
                <li>
                  <code>widget</code> (key <code>5</code> / <code>w</code>) &amp;{" "}
                  <code>memory</code> (key <code>6</code> / <code>m</code>): UI generation and{" "}
                  <code>/seance</code> vault indexing.
                </li>
                <li>
                  <code>fallback</code> (key <code>7</code> / <code>f</code>): Automatic failover
                  model retried transparently if the primary provider errors.
                </li>
              </ul>
            </div>
            <div className="code-snippet">
              /model → Tab switch pane · Alt+Left/Right filter kind · 1-7 bind role · Enter select
            </div>
          </article>

          <article className="bento-card">
            <div>
              <h3>Essential TUI slash commands</h3>
              <p>
                Every interactive workflow is accessible without leaving the keyboard:
              </p>
              <ul className="features">
                <li>
                  <code>/plan</code> &amp; <code>/goal</code>: Structured planning and autonomous
                  multi-turn goal loops.
                </li>
                <li>
                  <code>/undo</code> &amp; <code>/redo</code>: Revert or restore file checkpoints
                  step by step.
                </li>
                <li>
                  <code>/compact</code> &amp; <code>/context</code>: Inspect token budget and
                  compact earlier turns.
                </li>
                <li>
                  <code>/skills</code> &amp; <code>/mcp</code>: Browse, preview, and install skills
                  or probe MCP servers.
                </li>
                <li>
                  <code>/btw &lt;note&gt;</code>: Inject a live side-note into a running agent turn
                  without stopping it.
                </li>
              </ul>
            </div>
            <div className="code-snippet">Shift+Tab cycle permission mode · Ctrl+V paste</div>
          </article>
        </div>
      </section>

      <section className="section" aria-labelledby="architecture-deep-dive">
        <div className="section-head">
          <h2 id="architecture-deep-dive">5. Subsystems: Memory Vault, Telegram &amp; Studio</h2>
          <p className="meta">Obsidian vault identity, OpenClaw gateway, and Flux 60fps video</p>
        </div>

        <div className="bento-grid">
          <article className="bento-card">
            <div>
              <h3>Soul identity &amp; Obsidian MEMORY/ vault</h3>
              <p>
                Varynth maintains five deep persona files in <code>.varynth/</code> (
                <code>SOUL.md</code>, <code>AGENT.md</code>, <code>HEART.md</code>,{" "}
                <code>DREAM.md</code>, <code>USER.md</code>) alongside a structured Obsidian vault
                at <code>.varynth/MEMORY/</code> (<code>index.md</code>, <code>daily/</code>,{" "}
                <code>concepts/</code>, <code>decisions/</code>) with native{" "}
                <code>[[wikilink]]</code> resolution.
              </p>
            </div>
            <div className="code-snippet">.varynth/MEMORY/index.md → [[concepts/flux-pipeline]]</div>
          </article>

          <article className="bento-card">
            <div>
              <h3>OpenClaw Telegram gateway</h3>
              <p>
                When running <code>varynth serve</code> with a configured{" "}
                <code>telegram_bot_token</code> and <code>telegram_allow_from</code> allowlist,
                Varynth accepts remote commands (<code>/help</code>, <code>/status</code>,{" "}
                <code>/model</code>, <code>/todo</code>, <code>/undo</code>, <code>/redo</code>,{" "}
                <code>/clear</code>, <code>/revoke</code>), ingests photos and documents, and sends
                interactive inline approval buttons (<code>Approve Once</code>,{" "}
                <code>Always Approve</code>, <code>Deny</code>).
              </p>
            </div>
            <div className="code-snippet">$ varynth serve --host 127.0.0.1 --port 7420</div>
          </article>

          <article className="bento-card">
            <div>
              <h3>Flux image synthesis &amp; motion-frames 60fps</h3>
              <p>
                Authenticated accounts can invoke <code>POST /v1/images/generations</code> across
                5 worker gateways or use the interactive Flux Studio inside the Dashboard Console.
                Combined with <code>motion-frames-pro</code>, Varynth synthesizes hero assets and
                encodes deterministic 60fps MP4 launch videos via Playwright and FFmpeg.
              </p>
            </div>
            <div className="code-snippet">generate_image → Playwright virtual clock → FFmpeg</div>
          </article>
        </div>
      </section>

      <section className="section" aria-labelledby="enterprise-extensions">
        <div className="section-head">
          <h2 id="enterprise-extensions">
            6. Multi-agent swarm, personal API keys &amp; HTTPS provisioning
          </h2>
          <p className="meta">
            3-agent parallel orchestration, vk_live_ tokens, Git diff review, and Let&apos;s Encrypt
          </p>
        </div>

        <div className="bento-grid">
          <article className="bento-card">
            <div>
              <div className="skill-top">
                <h3 className="skill-title">Multi-agent swarm &amp; Git review</h3>
                <span className="tag tag-pro">cli swarm</span>
              </div>
              <p style={{ marginTop: "var(--space-xs)" }}>
                <code>varynth swarm &quot;&lt;task&gt;&quot;</code> dispatches your task through
                three specialized cognitive roles in sequence: <strong>Architect</strong> (
                <code>roles.plan</code>), <strong>Builder</strong> (<code>roles.default</code>),
                and <strong>Security Reviewer</strong> (<code>roles.advisor</code>). Use{" "}
                <code>varynth review --staged</code> before committing to catch race conditions and
                regressions.
              </p>
            </div>
            <div className="code-snippet">
              $ varynth swarm &quot;Implement rate-limited token bucket in Rust&quot;
            </div>
          </article>

          <article className="bento-card">
            <div>
              <div className="skill-top">
                <h3 className="skill-title">Personal API keys (vk_live_...)</h3>
                <span className="tag tag-free">bearer auth</span>
              </div>
              <p style={{ marginTop: "var(--space-xs)" }}>
                Mint long-lived <code>vk_live_...</code> tokens in your Dashboard Command Console.
                Only the SHA-256 digest is stored in PostgreSQL, and keys work everywhere{" "}
                <code>Authorization: Bearer vk_live_...</code> is accepted: OpenAI SDKs, Cursor,
                Continue, CI workflows, and <code>varynth sync</code>.
              </p>
            </div>
            <div className="code-snippet">
              Authorization: Bearer vk_live_9f8a7b6c5d4e3f2a1b0c...
            </div>
          </article>

          <article className="bento-card">
            <div>
              <div className="skill-top">
                <h3 className="skill-title">Automated Let&apos;s Encrypt HTTPS</h3>
                <span className="tag tag-ok">tls / ssl</span>
              </div>
              <p style={{ marginTop: "var(--space-xs)" }}>
                Provision automatic Let&apos;s Encrypt TLS certificates, HTTP/2, HSTS, and updates
                to <code>PUBLIC_ORIGIN</code> and <code>NEXT_PUBLIC_SITE_URL</code> on your VDS with
                a single idempotent command.
              </p>
            </div>
            <div className="code-snippet">
              # bash /opt/varynth/deploy/enable-https.sh varynth.yourdomain.com admin@yourdomain.com
            </div>
          </article>
        </div>
      </section>

      <footer className="foot">
        <div className="hero-actions" style={{ marginTop: 0 }}>
          <a className="btn" href="/register">
            Create your account
          </a>
          <a className="btn-quiet" href="/models">
            Browse models and skills
          </a>
          <a className="btn-quiet" href="/pricing">
            Compare Free and Pro
          </a>
        </div>
        <p>All commands and configuration paths apply to Varynth v0.1.3 on Windows, Linux, and macOS.</p>
      </footer>
    </main>
  );
}
