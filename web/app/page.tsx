import { InstallTabs } from "./components/InstallTabs";

export default function HomePage() {
  return (
    <main>
      <section className="hero">
        <div className="hero-split">
          <div>
            <h1>
              The model is <span className="serif-accent">already there.</span>
            </h1>
            <p className="claim">
              21 live verified Chat, Coding, Reasoning, and Vision models plus Flux Image Synthesis
              and Multi-Engine Web Search compiled into one sovereign Rust binary. Scroll with your
              mouse wheel to warp the singularity horizon.
            </p>
            <div className="hero-actions">
              <div className="install-pill" aria-label="Node.js install command">
                <span className="prompt">$</span>
                <span>npm i -g @zuziadev/varynth</span>
              </div>
              <a className="btn" href="/docs">
                Installation and docs
              </a>
              <a className="btn-quiet" href="/models">
                Explore 21 verified models &amp; skills
              </a>
              <a className="btn-quiet" href="/pricing">
                View plans
              </a>
            </div>
          </div>

          <div className="hero-singularity-viewport" aria-hidden="true" />
        </div>
      </section>

      <section className="bento-section" aria-labelledby="install-matrix-heading">
        <div className="section-intro">
          <h2 id="install-matrix-heading">
            Deploy on any workstation in <span className="serif-accent">one command.</span>
          </h2>
          <p className="claim">
            Switch between Windows PowerShell (<code>irm | iex</code>), Linux and macOS (
            <code>curl | bash</code>), Node.js (<code>npm</code> / <code>pnpm</code> /{" "}
            <code>bun</code>), and native Rust Cargo compilation.
          </p>
        </div>

        <InstallTabs />
      </section>

      <section className="bento-section" aria-labelledby="capabilities-heading">
        <div className="section-intro">
          <h2 id="capabilities-heading">
            Five subsystems in <span className="serif-accent">one local binary.</span>
          </h2>
          <p className="claim">
            Multi-role cognitive routing, Obsidian Markdown memory, Telegram remote control, and
            desktop overlays run natively without container bloat.
          </p>
        </div>

        <div className="bento-grid" data-slot="bento-grid" data-kind="product">
          <article className="bento-card span-2">
            <div>
              <h3>21 verified public models ready on launch</h3>
              <p>
                GPT-OSS 120B, Qwen 3.8 27B, Nemotron 3 120B, DeepSeek R1 Distill 32B, QwQ 32B,
                Llama 4 Scout, Llama 3.3 70B, Gemma 4 26B, and GLM 4.7 Flash served directly from
                the Varynth multi-gateway cluster with automatic failover.
              </p>
            </div>
            <div className="code-snippet">
              $ varynth --model varynth/gpt-oss-120b &quot;refactor src/routes.rs&quot;
            </div>
          </article>

          <article className="bento-card">
            <div>
              <h3>Obsidian MEMORY/ vault</h3>
              <p>
                Structured <code>SOUL.md</code>, <code>AGENT.md</code>, <code>HEART.md</code>,{" "}
                <code>DREAM.md</code>, and <code>.varynth/MEMORY/</code> notes with{" "}
                <code>[[wikilink]]</code> graph traversal and <code>/seance</code> recall.
              </p>
            </div>
            <div className="code-snippet">.varynth/MEMORY/index.md</div>
          </article>

          <article className="bento-card">
            <div>
              <h3>7-role /model selector</h3>
              <p>
                Split-pane browser to bind <code>default</code>, <code>plan</code>,{" "}
                <code>smol</code>, <code>advisor</code>, <code>widget</code>, <code>memory</code>,
                and <code>fallback</code> roles with instant <code>1-7</code> hotkeys.
              </p>
            </div>
            <div className="code-snippet">/model : keys 1-7 or d/p/s/a/w/m/f</div>
          </article>

          <article className="bento-card">
            <div>
              <h3>Desktop visual overlay</h3>
              <p>
                DPI-scaled aim halo, click ripples, typing status pill, and hardware{" "}
                <code>Esc</code> abort kill-switch for safe autonomous GUI operations.
              </p>
            </div>
            <div className="code-snippet">Varynth Operator Active [Esc=Abort]</div>
          </article>

          <article className="bento-card">
            <div>
              <h3>60fps Flux video studio</h3>
              <p>
                Synthesize Flux hero images via <code>generate_image</code> and pipe deterministic
                60fps HTML5 Canvas frames through Playwright and FFmpeg.
              </p>
            </div>
            <div className="code-snippet">tSec = frame / 60 : image2pipe : MP4</div>
          </article>
        </div>
      </section>

      <section className="stage">
        <div className="stage-grid">
          <p className="numeral" aria-hidden="true">
            01
          </p>
          <div>
            <h2>
              Open and code <span className="serif-accent">immediately.</span>
            </h2>
            <p className="claim">
              21 verified public models fetched from <code>/v1/models/public</code> with no
              registration required. Resume any interrupted terminal session with{" "}
              <code>varynth resume &lt;session-id&gt;</code>.
            </p>
          </div>
        </div>
      </section>

      <section className="stage">
        <div className="stage-grid">
          <p className="numeral" aria-hidden="true">
            02
          </p>
          <div>
            <h2>
              Sign in when you <span className="serif-accent">scale.</span>
            </h2>
            <p className="claim">
              <code>varynth login</code> completes the loopback RFC 7636 PKCE handshake, unlocking
              Pro quotas, Flux image synthesis, and automatic <code>SKILL.md</code> synchronization.
            </p>
          </div>
        </div>
      </section>

      <section className="plate">
        <div>
          <h2>
            Your key never becomes <span className="serif-accent">our key.</span>
          </h2>
          <p className="claim" style={{ marginTop: "var(--space-sm)" }}>
            Custom endpoints connect straight from your local Rust binary to your target gateway.
          </p>
        </div>
        <div className="instrument-panel" data-slot="plate-cli" data-kind="product">
          <div className="instrument-head">
            <span>~/.varynth/endpoints.toml</span>
            <span className="instrument-status">direct local socket</span>
          </div>
          <div className="instrument-body">
            <div className="term-line">
              <span className="term-cool">endpoint</span>
              <span className="term-cmd">https://your-gateway.internal/v1</span>
            </div>
            <div className="term-line">
              <span className="term-accent">secret</span>
              <span className="term-dim">stored strictly in OS keyring or local disk</span>
            </div>
            <div className="term-line">
              <span className="term-accent">egress</span>
              <span className="term-ok">client machine directly to target host</span>
            </div>
          </div>
        </div>
      </section>

      <section className="stage">
        <div className="stage-grid">
          <p className="numeral" aria-hidden="true">
            03
          </p>
          <div>
            <h2>
              Bring your own <span className="serif-accent">endpoint.</span>
            </h2>
            <p className="claim">
              <code>varynth endpoint add</code> keeps your custom URL and API key strictly on your
              machine, bypassing the Varynth cloud completely.
            </p>
          </div>
        </div>
      </section>

      <footer className="foot">
        <div className="hero-actions" style={{ marginTop: 0 }}>
          <a className="btn" href="/docs">
            Read installation docs
          </a>
          <a className="btn-quiet" href="/models">
            Browse 21 models &amp; skills
          </a>
          <a className="btn-quiet" href="/login">
            Sign in
          </a>
        </div>
        <p>Local by default. Zero provider prompt on first launch.</p>
      </footer>
    </main>
  );
}
