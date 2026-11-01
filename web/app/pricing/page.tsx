import { apiGet, PlansResponse, sessionToken, Usage } from "../../lib/api";
import { PlanActions } from "../plan-actions";

export const dynamic = "force-dynamic";

function QuotaTable({ plan }: { plan: PlansResponse["data"][number] }) {
  return (
    <table className="quota">
      <caption className="visually-hidden">{plan.name} limits</caption>
      <tbody>
        <tr>
          <th scope="row">Chat, per day</th>
          <td>{plan.quota.chat_per_day}</td>
        </tr>
        <tr>
          <th scope="row">Chat, per minute</th>
          <td>{plan.quota.chat_per_minute}</td>
        </tr>
        <tr>
          <th scope="row">Search, per day</th>
          <td>{plan.quota.search_per_day}</td>
        </tr>
      </tbody>
    </table>
  );
}

export default async function PricingPage() {
  const token = await sessionToken();
  const [plansResult, usageResult] = await Promise.all([
    apiGet<PlansResponse>("/v1/plans"),
    token ? apiGet<Usage>("/v1/me/usage", { token }) : Promise.resolve(null),
  ]);

  if (!plansResult.ok) {
    return (
      <main className="page">
        <header className="page-head">
          <h1>Pricing</h1>
          <p className="claim">Two plans. The numbers come from the server that enforces them.</p>
        </header>
        <div className="panel panel-error" role="alert">
          <p className="panel-title">The plan list did not load.</p>
          <p className="meta">{plansResult.message}</p>
          <a className="text-link" href="/pricing">
            Try again
          </a>
        </div>
      </main>
    );
  }

  const plans = plansResult.data.data;
  const billing = plansResult.data.billing_configured;
  const currentPlan = usageResult?.ok ? usageResult.data.plan : null;

  return (
    <main className="page">
      <header className="page-head">
        <h1>Pricing and tier limits</h1>
        <p className="claim">
          Server-enforced quotas for your account, while custom BYOK endpoints and local tools
          remain completely unmetered.
        </p>
      </header>

      <section className="section" aria-labelledby="plans">
        <div className="section-head">
          <h2 id="plans">Account plans</h2>
          <p className="meta">
            {billing ? "self-service changes enabled" : "plans are set by the operator"}
          </p>
        </div>
        <div className="plans">
          {plans.map((plan) => {
            const isCurrent = currentPlan === plan.id;
            const isPro = plan.id === "pro";
            const extraFeatures = isPro
              ? [
                  "Access to 146 Pro Models (Claude Opus 4.8/4.7, Sonnet 5/4.6, GPT-5.6 Luna/Terra, DeepSeek V4.1, Gemini 3.5, Grok 4.3, Flux Ultra)",
                  "All 6 Pro Agent Skills (motion-frames-pro, create-mcp, security-auditor, and more)",
                  "Flux image synthesis (/v1/images/generations) via built-in generate_image tool",
                  "Automated SKILL.md synchronization to ~/.varynth/skills/ on varynth login",
                  "OpenClaw Telegram remote gateway & 60fps Playwright/FFmpeg video pipeline",
                ]
              : [
                  "Access to 21 Free Models (GPT-OSS 120B/20B, Qwen 3.8/30B, Llama 4 Scout, Llama 3.3 70B, Gemma 4, GLM 4.7 Flash, DeepSeek R1 Distill)",
                  "6 Core Agent Skills (computer-use, motion-frames, cli-design, and more)",
                  "Full 7-role /model routing and Obsidian .varynth/MEMORY/ vault",
                  "Unmetered custom endpoints via varynth endpoint add",
                ];
            return (
              <section
                className="plan"
                key={plan.id}
                aria-labelledby={`plan-${plan.id}`}
                data-current={isCurrent ? "true" : undefined}
                data-featured={isPro ? "true" : undefined}
              >
                <div>
                  <div className="plan-head">
                    <h3 id={`plan-${plan.id}`}>{plan.name}</h3>
                    <div style={{ display: "flex", gap: "var(--space-xs)" }}>
                      {isPro ? <span className="tag tag-pro">full studio</span> : null}
                      {isCurrent ? <span className="tag tag-free">your plan</span> : null}
                    </div>
                  </div>
                  <p className="plan-price">
                    {plan.price_per_month === 0
                      ? "$0 per month"
                      : plan.price_per_month === null
                        ? "Pro tier, operator or instant stub"
                        : `$${plan.price_per_month} per month`}
                  </p>
                  <QuotaTable plan={plan} />
                  <ul className="features">
                    {plan.features.map((feature) => (
                      <li key={feature}>{feature}</li>
                    ))}
                    {extraFeatures.map((extra) => (
                      <li key={extra}>{extra}</li>
                    ))}
                  </ul>
                </div>

                <div>
                  {token && billing ? (
                    <PlanActions
                      currentPlan={currentPlan ?? ""}
                      target={plan.id === "pro" ? "pro" : "free"}
                      label={plan.id === "pro" ? "Upgrade to Pro" : "Switch to Free"}
                    />
                  ) : null}
                  {token && !billing ? (
                    <p className="provision">
                      Self-service changes are off on this server. Your plan is assigned by the
                      operator; this page never charges a card.
                    </p>
                  ) : null}
                  {!token ? (
                    <a className="text-link" href="/login?next=/pricing">
                      Sign in to choose
                    </a>
                  ) : null}
                </div>
              </section>
            );
          })}
        </div>
      </section>

      <section className="plate" aria-labelledby="without-account">
        <h2 id="without-account">Without an account</h2>
        <p className="claim">
          {plansResult.data.anonymous.chat_per_day} chats a day,{" "}
          {plansResult.data.anonymous.chat_per_minute} per minute on the Varynth public models right
          from your terminal with zero sign-in. Web search, Flux image synthesis, and Pro skills
          activate when you sign in.
        </p>
      </section>

      <section className="section" aria-labelledby="install-after-plan">
        <div className="section-head">
          <h2 id="install-after-plan">Install and sync your plan</h2>
          <p className="meta">Windows, Linux, macOS, Node.js, and Rust</p>
        </div>
        <div className="bento-grid">
          <div className="bento-card">
            <div>
              <h3>Windows PowerShell</h3>
              <p>Install via PowerShell and bind your account with PKCE login.</p>
            </div>
            <div className="code-snippet">irm https://varynth.zuzia.dev/install.ps1 | iex</div>
          </div>
          <div className="bento-card">
            <div>
              <h3>Linux &amp; macOS</h3>
              <p>Install via POSIX shell or global npm/cargo and run login.</p>
            </div>
            <div className="code-snippet">curl -fsSL https://varynth.zuzia.dev/install.sh | sh</div>
          </div>
          <div className="bento-card">
            <div>
              <h3>Node.js &amp; Rust</h3>
              <p>Install globally with npm or build from Git with Cargo.</p>
            </div>
            <div className="code-snippet">npm i -g @zuziadev/varynth &amp;&amp; varynth login</div>
          </div>
        </div>
      </section>

      <footer className="foot">
        <div className="hero-actions" style={{ marginTop: 0 }}>
          <a className="text-link" href="/models">
            See the models and skills
          </a>
          <a className="btn-quiet" href="/docs">
            Full installation docs
          </a>
        </div>
        <p>
          {billing
            ? "This server runs the billing stub: a plan change updates your account immediately and takes no payment."
            : "No payment provider is connected to this server. Nothing here can take a charge."}
        </p>
      </footer>
    </main>
  );
}
