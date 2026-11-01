import {
  apiGet,
  CatalogProvider,
  PublicModel,
  sessionToken,
  SkillsCatalogResponse,
  type Result,
} from "../../lib/api";
import { ModelsExplorer } from "../components/ModelsExplorer";

export const dynamic = "force-dynamic";

function ErrorPanel({ title, result }: { title: string; result: Result<unknown> }) {
  return (
    <div className="panel panel-error" role="alert">
      <p className="panel-title">{title}</p>
      <p className="meta">{result.ok ? "" : result.message}</p>
    </div>
  );
}

export default async function ModelsPage() {
  const token = await sessionToken();
  const [publicResult, catalogResult, skillsResult] = await Promise.all([
    apiGet<{ object: string; data: PublicModel[] }>("/v1/models/public"),
    token
      ? apiGet<{ providers: CatalogProvider[] }>("/v1/catalog", { token })
      : Promise.resolve(null),
    apiGet<SkillsCatalogResponse>("/v1/catalog/skills", { token }),
  ]);
  const publicModels = publicResult.ok ? publicResult.data.data : [];
  const providers = catalogResult?.ok ? catalogResult.data.providers : [];
  const skills = skillsResult.ok ? skillsResult.data.skills ?? skillsResult.data.data ?? [] : [];
  const activeTier = skillsResult.ok ? skillsResult.data.tier : "free";

  return (
    <main className="page">
      <header className="page-head">
        <h1>
          Models, role routing, and <span className="serif-accent">agent skills.</span>
        </h1>
        <p className="claim">
          {publicModels.length > 0 ? publicModels.length : 167} live verified upstream models (21 Free + 146 Pro) across
          Flagship Reasoning, Coding, Vision &amp; Multimodal Chat, and Low-Latency Tool Execution,
          plus 7 cognitive role bindings and the tiered skills library.
        </p>
      </header>

      <section className="section" aria-labelledby="public-models">
        <div className="section-head">
          <h2 id="public-models">Varynth public models</h2>
          <p className="meta">
            {publicResult.ok ? `${publicModels.length} verified & active` : "unavailable"}, no
            account needed — click any row to copy its model ID
          </p>
        </div>
        {!publicResult.ok ? (
          <ErrorPanel title="The public list did not load." result={publicResult} />
        ) : publicModels.length === 0 ? (
          <div className="panel">
            <p className="panel-title">No public models are enabled yet.</p>
            <p className="meta">
              This server has not published its Varynth models. The account catalog below is
              unaffected.
            </p>
          </div>
        ) : (
          <ModelsExplorer models={publicModels} />
        )}
      </section>

      <section className="section" aria-labelledby="role-routing">
        <div className="section-head">
          <h2 id="role-routing">Multi-role /model binding matrix</h2>
          <p className="meta">7 specialized slots in ~/.varynth/config.toml</p>
        </div>
        <div className="bento-grid">
          <article className="bento-card">
            <div>
              <h3>1. Default &amp; 2. Plan</h3>
              <p>
                <code>default</code> drives daily coding and multi-round tool execution (up to 128
                rounds per turn). <code>plan</code> handles architectural synthesis when you invoke{" "}
                <code>/plan</code> or cycle into Plan mode with <code>Shift+Tab</code>.
              </p>
            </div>
            <div className="code-snippet">/model : press 1 (default) or 2 (plan)</div>
          </article>
          <article className="bento-card">
            <div>
              <h3>3. Smol &amp; 4. Advisor</h3>
              <p>
                <code>smol</code> runs low-latency context compaction (<code>/compact</code>) and
                session titling. <code>advisor</code> acts as an independent verification auditor
                for <code>/review</code> and autonomous <code>/goal</code> loops.
              </p>
            </div>
            <div className="code-snippet">/model : press 3 (smol) or 4 (advisor)</div>
          </article>
          <article className="bento-card">
            <div>
              <h3>5. Widget, 6. Memory &amp; 7. Fallback</h3>
              <p>
                <code>widget</code> and <code>memory</code> power UI artifacts and Obsidian{" "}
                <code>.varynth/MEMORY/</code> indexing, while <code>fallback</code> automatically
                takes over if the primary provider returns an upstream error.
              </p>
            </div>
            <div className="code-snippet">/model : press 5, 6, or 7 (fallback)</div>
          </article>
        </div>
      </section>

      <section className="section" aria-labelledby="skills-catalog">
        <div className="section-head">
          <h2 id="skills-catalog">Tiered agent skills catalog</h2>
          <p className="meta">
            {skillsResult.ok
              ? `${skills.length} skills, active tier: ${activeTier}`
              : "unavailable"}
          </p>
        </div>
        {!skillsResult.ok ? (
          <ErrorPanel title="The skills catalog did not load." result={skillsResult} />
        ) : skills.length === 0 ? (
          <div className="panel">
            <p className="panel-title">No skills published yet.</p>
            <p className="meta">Run migrations to seed the Free and Pro skills catalog.</p>
          </div>
        ) : (
          <div className="skills-grid">
            {skills.map((skill) => (
              <article
                className="skill-card"
                key={skill.id}
                data-tier={skill.tier}
              >
                <div>
                  <div className="skill-top">
                    <div>
                      <h3 className="skill-title">{skill.name}</h3>
                      <div className="skill-id">.agents/skills/{skill.id}</div>
                    </div>
                    <span className={`tag ${skill.tier === "pro" ? "tag-pro" : "tag-free"}`}>
                      {skill.tier}
                    </span>
                  </div>
                  <p className="skill-desc">
                    {skill.description}
                  </p>
                </div>
                <div className="skill-foot">
                  <span>
                    {skill.unlocked ? "Entitled and active" : "Requires Pro plan"}
                  </span>
                  <span className={`tag ${skill.unlocked ? "tag-free" : "tag-locked"}`}>
                    {skill.unlocked ? "ready" : "locked"}
                  </span>
                </div>
              </article>
            ))}
          </div>
        )}
      </section>

      <section className="section" aria-labelledby="account-catalog">
        <div className="section-head">
          <h2 id="account-catalog">Provider-grouped account catalog</h2>
          <p className="meta">
            {token ? "signed in" : "sign in to load"}, 9 active provider families
          </p>
        </div>
        {!token ? (
          <div className="panel">
            <p className="panel-title">Sign in to browse models grouped by provider family.</p>
            <p className="meta">
              Signed-in accounts can call any verified model by its provider-prefixed identifier (
              <code>openai/gpt-oss-120b</code>, <code>qwen/qwen3.8-27b</code>,{" "}
              <code>nvidia/nemotron-3-120b-a12b</code>,{" "}
              <code>deepseek/deepseek-r1-distill-qwen-32b</code>,{" "}
              <code>meta/llama-4-scout-17b-16e-instruct</code>, and more).
            </p>
            <a className="text-link" href="/login?next=/models">
              Sign in
            </a>
          </div>
        ) : catalogResult && !catalogResult.ok ? (
          <ErrorPanel title="The account catalog did not load." result={catalogResult} />
        ) : providers.length === 0 ? (
          <div className="panel">
            <p className="panel-title">Your catalog is empty.</p>
            <p className="meta">
              No provider models are configured for accounts on this server.
            </p>
          </div>
        ) : (
          <div className="groups">
            {providers.map((provider) => (
              <section
                className="group"
                key={provider.id}
                aria-labelledby={`provider-${provider.id}`}
              >
                <div className="group-head">
                  <h3 id={`provider-${provider.id}`}>{provider.name}</h3>
                  <p className="meta">
                    {provider.kind}, {provider.models.length}{" "}
                    {provider.models.length === 1 ? "model" : "models"}
                  </p>
                </div>
                {provider.models.length === 0 ? (
                  <p className="meta">No models under this provider.</p>
                ) : (
                  <ul className="rows">
                    {provider.models.map((model) => (
                      <li className="row" key={`${provider.id}/${model.id}`}>
                        <span className="row-id">
                          {provider.id}/{model.id}
                        </span>
                        <span className="row-name">{model.name}</span>
                        <span className="tag">account</span>
                      </li>
                    ))}
                  </ul>
                )}
              </section>
            ))}
          </div>
        )}
      </section>

      <footer className="foot">
        <div className="hero-actions" style={{ marginTop: 0 }}>
          <a className="text-link" href="/pricing">
            Compare plans and Pro skills
          </a>
          <a className="btn-quiet" href="/docs">
            Read CLI &amp; install docs
          </a>
        </div>
        <p>Model and skill identifiers above are the exact strings the Varynth CLI and API accept.</p>
      </footer>
    </main>
  );
}
