"use client";

import React, { useState } from "react";
import type {
  ApiKeyItem,
  CliSessionItem,
  Me,
  PublicModel,
  SkillItem,
  Usage,
} from "../../lib/api";
import { PlanActions } from "../plan-actions";
import { SignOutButton } from "../sign-out-button";
import { InstallTabs } from "./InstallTabs";
import { ByokVault } from "./ByokVault";

interface DashboardConsoleProps {
  me: Me | null;
  meError?: string;
  initialUsage: Usage | null;
  usageError?: string;
  billingConfigured: boolean;
  skills: SkillItem[];
  models: PublicModel[];
  initialKeys: ApiKeyItem[];
  initialSessions: CliSessionItem[];
  sessionJwt: string;
  gatewayOrigin: string;
}

type ConsoleTab =
  | "playground"
  | "byok"
  | "keys"
  | "sessions"
  | "router"
  | "skills"
  | "sdk"
  | "workstation";

type PlaygroundMode = "chat" | "image" | "search";

interface RoleAssignment {
  role: string;
  hotkey: string;
  title: string;
  desc: string;
  defaultModel: string;
}

const ROLE_DEFS: RoleAssignment[] = [
  {
    role: "default",
    hotkey: "1 / d",
    title: "Default Coding Engine",
    desc: "Primary autonomous file editing, bash execution, and 128-round tool loop.",
    defaultModel: "varynth/gpt-oss-120b",
  },
  {
    role: "plan",
    hotkey: "2 / p",
    title: "Architectural Planner",
    desc: "Deep multi-file system design, /plan blueprints, and structural reasoning.",
    defaultModel: "varynth/qwen3.8-27b",
  },
  {
    role: "smol",
    hotkey: "3 / s",
    title: "Smol Fast Sub-Agent",
    desc: "Low-latency repository search, AST indexing, and parallel sub-agent sweeps.",
    defaultModel: "varynth/gpt-oss-20b",
  },
  {
    role: "advisor",
    hotkey: "4 / a",
    title: "Security & Audit Advisor",
    desc: "Second-opinion verification, race-condition detection, and diff review.",
    defaultModel: "varynth/deepseek-r1-distill-qwen-32b",
  },
  {
    role: "widget",
    hotkey: "5 / w",
    title: "TUI Status & Widget",
    desc: "Real-time terminal telemetry, commit message synthesis, and quick hints.",
    defaultModel: "varynth/glm-4.7-flash",
  },
  {
    role: "memory",
    hotkey: "6 / m",
    title: "Obsidian Memory Curator",
    desc: "Maintains .varynth/MEMORY/ wikilink notes, SOUL.md, and /dream consolidation.",
    defaultModel: "varynth/qwen3-30b-a3b-fp8",
  },
  {
    role: "fallback",
    hotkey: "7 / f",
    title: "Cluster Failover",
    desc: "Automatic standby model when the primary upstream gateway is saturated.",
    defaultModel: "varynth/nemotron-3-120b-a12b",
  },
];

const QUICK_PROMPTS = [
  {
    label: "Rust lock-free ring buffer",
    prompt: "Write a concise lock-free SPSC ring buffer in Rust using AtomicUsize with acquire/release ordering.",
  },
  {
    label: "SQLite WAL concurrency tuning",
    prompt: "Give me the exact PRAGMA settings for high-throughput concurrent reads and writes in SQLite WAL mode.",
  },
  {
    label: "7-role routing strategy",
    prompt: "Explain how splitting coding tasks across plan, default, smol, and advisor models reduces latency and token waste.",
  },
];

const IMAGE_MODELS = [
  { id: "varynth/flux-1-schnell", name: "Flux 1 Schnell (Ultra-Fast 4-Step)" },
  { id: "varynth/flux-2-klein-9b", name: "Flux 2 Klein 9B (High-Detail Studio)" },
  { id: "varynth/flux-2-klein-4b", name: "Flux 2 Klein 4B (Balanced)" },
  { id: "varynth/phoenix-1.0", name: "Leonardo Phoenix 1.0 (Photorealistic)" },
  { id: "varynth/lucid-origin", name: "Leonardo Lucid Origin (Artistic)" },
  { id: "varynth/stable-diffusion-xl-lightning", name: "ByteDance SDXL Lightning (Fast)" },
];

const IMAGE_QUICK_PROMPTS = [
  {
    label: "Gargantua Obsidian Monolith",
    prompt: "Minimalist obsidian glass monolith floating above a golden relativistic black hole horizon, volumetric accretion disk, 8k cinematic render",
  },
  {
    label: "Cybernetic Rust Core",
    prompt: "Precision machined titanium and copper neural processor core with glowing amber fiber optics, macro studio photography",
  },
];

const SEARCH_PROVIDERS = [
  { id: "auto", name: "Auto Failover (Tavily → Brave → Exa → Serper)" },
  { id: "tavily", name: "Tavily AI Research" },
  { id: "brave", name: "Brave Independent Index" },
  { id: "exa", name: "Exa Neural Search" },
  { id: "serper", name: "Serper Google Index" },
];

export function DashboardConsole({
  me,
  meError,
  initialUsage,
  usageError,
  billingConfigured,
  skills,
  models,
  initialKeys,
  initialSessions,
  sessionJwt,
  gatewayOrigin,
}: DashboardConsoleProps) {
  const [usage, setUsage] = useState<Usage | null>(initialUsage);
  const [refreshing, setRefreshing] = useState(false);
  const [lastRefreshed, setLastRefreshed] = useState<string>("Live");
  const [activeTab, setActiveTab] = useState<ConsoleTab>("playground");

  // Identity & Token Vault state
  const [copiedId, setCopiedId] = useState(false);
  const [showToken, setShowToken] = useState(false);
  const [copiedToken, setCopiedToken] = useState(false);

  // Personal API Keys (vk_live_...) state
  const [apiKeys, setApiKeys] = useState<ApiKeyItem[]>(initialKeys);
  const [newKeyName, setNewKeyName] = useState<string>("Production CLI & CI");
  const [mintingKey, setMintingKey] = useState(false);
  const [mintedSecretKey, setMintedSecretKey] = useState<string | null>(null);
  const [copiedSecretKey, setCopiedSecretKey] = useState(false);
  const [keyError, setKeyError] = useState<string | null>(null);

  // CLI Sessions state
  const [cliSessions, setCliSessions] = useState<CliSessionItem[]>(initialSessions);
  const [syncingSession, setSyncingSession] = useState(false);
  const [copiedSessionCmd, setCopiedSessionCmd] = useState<string | null>(null);

  // Multi-Modal Playground state
  const [pgMode, setPgMode] = useState<PlaygroundMode>("chat");
  const [selectedModel, setSelectedModel] = useState<string>(
    models[0]?.id || "varynth/gpt-oss-120b"
  );
  const [systemPrompt, setSystemPrompt] = useState<string>(
    "You are Varynth, a sovereign Rust coding runtime. Provide concise, production-ready technical answers."
  );
  const [userPrompt, setUserPrompt] = useState<string>(QUICK_PROMPTS[0].prompt);
  const [imageModel, setImageModel] = useState<string>(IMAGE_MODELS[0].id);
  const [imagePrompt, setImagePrompt] = useState<string>(IMAGE_QUICK_PROMPTS[0].prompt);
  const [searchProvider, setSearchProvider] = useState<string>("auto");
  const [searchQuery, setSearchQuery] = useState<string>(
    "Rust axum 0.8 tower-http high concurrency best practices"
  );
  const [pgRunning, setPgRunning] = useState(false);
  const [pgOutput, setPgOutput] = useState<string>("");
  const [pgImageSrc, setPgImageSrc] = useState<string | null>(null);
  const [copiedImgB64, setCopiedImgB64] = useState(false);
  const [pgSearchResults, setPgSearchResults] = useState<
    Array<{ title?: string; url?: string; content?: string; snippet?: string }>
  >([]);
  const [pgLatency, setPgLatency] = useState<number | null>(null);
  const [pgError, setPgError] = useState<string | null>(null);

  const handleDownloadImage = () => {
    if (!pgImageSrc) return;
    try {
      if (pgImageSrc.startsWith("data:")) {
        const [meta, b64] = pgImageSrc.split(",");
        const mimeMatch = meta.match(/data:(.*?);base64/);
        const mime = mimeMatch ? mimeMatch[1] : "image/png";
        const ext =
          mime.includes("jpeg") || mime.includes("jpg")
            ? "jpg"
            : mime.includes("webp")
              ? "webp"
              : "png";
        const binary = atob(b64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
          bytes[i] = binary.charCodeAt(i);
        }
        const blob = new Blob([bytes], { type: mime });
        const blobUrl = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = blobUrl;
        a.download = `varynth-${imageModel.replace("varynth/", "")}-${Date.now()}.${ext}`;
        document.body.appendChild(a);
        a.click();
        a.remove();
        setTimeout(() => URL.revokeObjectURL(blobUrl), 2000);
        return;
      }
      const a = document.createElement("a");
      a.href = pgImageSrc;
      a.download = `varynth-image-${Date.now()}.png`;
      a.click();
    } catch {
      // Fallback
    }
  };

  // 7-Role Router state
  const [roleMap, setRoleMap] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {};
    ROLE_DEFS.forEach((r) => {
      init[r.role] = r.defaultModel;
    });
    return init;
  });
  const [copiedToml, setCopiedToml] = useState(false);

  // Skills Vault state
  const [selectedSkill, setSelectedSkill] = useState<SkillItem | null>(
    skills[0] || null
  );
  const [copiedSkill, setCopiedSkill] = useState(false);

  // SDK snippet state
  const [sdkLang, setSdkLang] = useState<"curl" | "ts" | "python" | "flux" | "search">("curl");
  const [copiedSdk, setCopiedSdk] = useState(false);

  const unlockedSkills = skills.filter((s) => s.unlocked);
  const activeApiKeys = apiKeys.filter((k) => !k.revoked);

  const handleRefreshTelemetry = async () => {
    setRefreshing(true);
    try {
      const [usageRes, keysRes, sessionsRes] = await Promise.all([
        fetch("/api/playground", { cache: "no-store" }),
        fetch("/api/keys", { cache: "no-store" }),
        fetch("/api/sessions", { cache: "no-store" }),
      ]);
      if (usageRes.ok) {
        const data = (await usageRes.json()) as Usage;
        setUsage(data);
      }
      if (keysRes.ok) {
        const kData = (await keysRes.json()) as { data?: ApiKeyItem[] };
        if (kData.data) setApiKeys(kData.data);
      }
      if (sessionsRes.ok) {
        const sData = (await sessionsRes.json()) as { data?: CliSessionItem[] };
        if (sData.data) setCliSessions(sData.data);
      }
      const now = new Date();
      setLastRefreshed(now.toISOString().slice(11, 19) + " UTC");
    } finally {
      setRefreshing(false);
    }
  };

  const handleRunPlayground = async (e: React.FormEvent) => {
    e.preventDefault();
    if (pgRunning) return;
    setPgRunning(true);
    setPgError(null);
    setPgOutput("");
    setPgImageSrc(null);
    setPgSearchResults([]);
    setPgLatency(null);

    try {
      const payload =
        pgMode === "image"
          ? { mode: "image", model: imageModel, prompt: imagePrompt }
          : pgMode === "search"
            ? { mode: "search", provider: searchProvider, query: searchQuery }
            : {
                mode: "chat",
                model: selectedModel,
                system: systemPrompt,
                prompt: userPrompt,
              };

      const res = await fetch("/api/playground", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) {
        setPgError(
          data?.error?.message ||
            data?.completion?.error?.message ||
            `Gateway returned HTTP ${res.status}`
        );
      } else {
        setPgLatency(data.latency_ms ?? null);
        if (pgMode === "image") {
          const item =
            data?.completion?.data?.[0] ||
            data?.result?.data?.[0] ||
            data?.data?.[0] ||
            data?.completion ||
            data?.result;
          const rawB64: string | undefined = item?.b64_json || item?.image;
          if (typeof rawB64 === "string" && rawB64.trim().length > 0) {
            const cleanB64 = rawB64
              .replace(/^data:image\/[a-zA-Z0-9+.-]+;base64,/, "")
              .replace(/\s+/g, "");
            const mime =
              item?.mime_type ||
              (cleanB64.startsWith("/9j/")
                ? "image/jpeg"
                : cleanB64.startsWith("UklGR")
                  ? "image/webp"
                  : "image/png");
            setPgImageSrc(`data:${mime};base64,${cleanB64}`);
          } else if (typeof item?.url === "string" && item.url.trim().length > 0) {
            setPgImageSrc(item.url.trim());
          } else {
            setPgOutput(JSON.stringify(data?.completion ?? data?.result ?? data, null, 2));
          }
        } else if (pgMode === "search") {
          const results =
            data?.completion?.results ||
            data?.completion?.hits ||
            data?.completion?.data ||
            data?.result?.results ||
            data?.result?.data ||
            [];
          if (Array.isArray(results) && results.length > 0) {
            setPgSearchResults(results);
          } else {
            setPgOutput(JSON.stringify(data?.completion ?? data?.result ?? data, null, 2));
          }
        } else {
          const text =
            data?.completion?.choices?.[0]?.message?.content ||
            JSON.stringify(data?.completion ?? data, null, 2);
          setPgOutput(text);
        }
        if (data.usage) {
          setUsage(data.usage as Usage);
          const now = new Date();
          setLastRefreshed(now.toISOString().slice(11, 19) + " UTC");
        }
      }
    } catch {
      setPgError("Network error while contacting /api/playground.");
    } finally {
      setPgRunning(false);
    }
  };

  const handleCreateApiKey = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!newKeyName.trim() || mintingKey) return;
    setMintingKey(true);
    setKeyError(null);
    setMintedSecretKey(null);
    try {
      const res = await fetch("/api/keys", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ name: newKeyName.trim() }),
      });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) {
        setKeyError(data?.error?.message || `Could not create API key (${res.status}).`);
      } else {
        setMintedSecretKey(data.key || null);
        const listRes = await fetch("/api/keys", { cache: "no-store" });
        if (listRes.ok) {
          const listData = (await listRes.json()) as { data?: ApiKeyItem[] };
          if (listData.data) setApiKeys(listData.data);
        }
      }
    } catch {
      setKeyError("Network error while creating API key.");
    } finally {
      setMintingKey(false);
    }
  };

  const handleRevokeApiKey = async (id: string) => {
    try {
      const res = await fetch("/api/keys", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ action: "revoke", id }),
      });
      if (res.ok) {
        setApiKeys((prev) =>
          prev.map((k) => (k.id === id ? { ...k, revoked: true } : k))
        );
      }
    } catch {
      // Ignore network hiccup
    }
  };

  const handleSyncDemoSession = async () => {
    if (syncingSession) return;
    setSyncingSession(true);
    try {
      const sessionKey = `sess_${Math.random().toString(36).slice(2, 10)}`;
      const res = await fetch("/api/sessions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          session_key: sessionKey,
          title: "Multi-agent swarm: zero-copy gateway refactor",
          model: selectedModel,
          workspace: "~/projects/varynth-core",
          message_count: 18,
          preview:
            "[Architect + Builder + Reviewer] Verified Binet RK4 geodesic shader and 5-gateway failover pipeline.",
        }),
      });
      if (res.ok) {
        const listRes = await fetch("/api/sessions", { cache: "no-store" });
        if (listRes.ok) {
          const sData = (await listRes.json()) as { data?: CliSessionItem[] };
          if (sData.data) setCliSessions(sData.data);
        }
      }
    } finally {
      setSyncingSession(false);
    }
  };

  const handleDeleteSession = async (sessionKey: string) => {
    try {
      const res = await fetch("/api/sessions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ action: "delete", session_key: sessionKey }),
      });
      if (res.ok) {
        setCliSessions((prev) => prev.filter((s) => s.session_key !== sessionKey));
      }
    } catch {
      // Ignore
    }
  };

  const generatedToml = [
    "# ~/.varynth/config.toml — Generated by Varynth Console",
    `api_base = "${gatewayOrigin}/v1"`,
    `default_model = "${roleMap.default}"`,
    "max_tool_rounds = 128",
    "",
    "[roles]",
    `default  = "${roleMap.default}"`,
    `plan     = "${roleMap.plan}"`,
    `smol     = "${roleMap.smol}"`,
    `advisor  = "${roleMap.advisor}"`,
    `widget   = "${roleMap.widget}"`,
    `memory   = "${roleMap.memory}"`,
    `fallback = "${roleMap.fallback}"`,
    "",
    "[memory]",
    'vault_dir = ".varynth/MEMORY"',
    "auto_dream = true",
  ].join("\n");

  const handleDownloadToml = () => {
    const blob = new Blob([generatedToml], { type: "text/plain;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "config.toml";
    a.click();
    URL.revokeObjectURL(url);
  };

  const tokenDisplay = showToken
    ? sessionJwt
    : sessionJwt
      ? `${sessionJwt.slice(0, 12)}••••••••••••••••••••••••••••${sessionJwt.slice(-8)}`
      : "No session token";

  const getSdkSnippet = () => {
    const tokenPlaceholder =
      mintedSecretKey || (showToken ? sessionJwt : "vk_live_YOUR_API_KEY");
    if (sdkLang === "curl") {
      return [
        `curl -fsSL ${gatewayOrigin}/v1/chat/completions \\`,
        `  -H "Authorization: Bearer ${tokenPlaceholder}" \\`,
        `  -H "Content-Type: application/json" \\`,
        `  -d '{`,
        `    "model": "${selectedModel}",`,
        `    "messages": [`,
        `      {"role": "system", "content": "You are Varynth."},`,
        `      {"role": "user", "content": "Refactor src/routes.rs for zero-allocation parsing."}`,
        `    ]`,
        `  }'`,
      ].join("\n");
    }
    if (sdkLang === "ts") {
      return [
        `import OpenAI from "openai";`,
        ``,
        `const varynth = new OpenAI({`,
        `  baseURL: "${gatewayOrigin}/v1",`,
        `  apiKey: process.env.VARYNTH_API_KEY ?? "${tokenPlaceholder}",`,
        `});`,
        ``,
        `const response = await varynth.chat.completions.create({`,
        `  model: "${selectedModel}",`,
        `  messages: [{ role: "user", content: "Audit our PostgreSQL connection pool." }],`,
        `});`,
        `console.log(response.choices[0]?.message?.content);`,
      ].join("\n");
    }
    if (sdkLang === "python") {
      return [
        `from openai import OpenAI`,
        ``,
        `client = OpenAI(`,
        `    base_url="${gatewayOrigin}/v1",`,
        `    api_key="${tokenPlaceholder}",`,
        `)`,
        ``,
        `completion = client.chat.completions.create(`,
        `    model="${selectedModel}",`,
        `    messages=[{"role": "user", "content": "Optimize this Tokio task spawner."}],`,
        `)`,
        `print(completion.choices[0].message.content)`,
      ].join("\n");
    }
    if (sdkLang === "flux") {
      return [
        `curl -fsSL ${gatewayOrigin}/v1/images/generations \\`,
        `  -H "Authorization: Bearer ${tokenPlaceholder}" \\`,
        `  -H "Content-Type: application/json" \\`,
        `  -d '{`,
        `    "model": "varynth/flux-1-schnell",`,
        `    "prompt": "Minimalist obsidian glass monolith floating above a golden black hole horizon, 8k",`,
        `    "n": 1`,
        `  }'`,
      ].join("\n");
    }
    return [
      `curl -fsSL ${gatewayOrigin}/v1/search \\`,
      `  -H "Authorization: Bearer ${tokenPlaceholder}" \\`,
      `  -H "Content-Type: application/json" \\`,
      `  -d '{`,
      `    "query": "Rust axum 0.8 tower-http cors middleware",`,
      `    "provider": "auto"`,
      `  }'`,
    ].join("\n");
  };

  const chatUsed = usage?.today.chat ?? 0;
  const chatLimit = usage?.limits.chat_per_day ?? 50;
  const chatPct = chatLimit > 0 ? Math.min(100, Math.round((chatUsed / chatLimit) * 100)) : 0;

  const searchUsed = usage?.today.search ?? 0;
  const searchLimit = usage?.limits.search_per_day ?? 15;
  const searchPct = searchLimit > 0 ? Math.min(100, Math.round((searchUsed / searchLimit) * 100)) : 0;

  const burstUsed = usage?.today.chat_this_minute ?? 0;
  const burstLimit = usage?.limits.chat_per_minute ?? 5;
  const burstPct = burstLimit > 0 ? Math.min(100, Math.round((burstUsed / burstLimit) * 100)) : 0;

  return (
    <div className="dash-console">
      {/* Top Header & Live Telemetry Status Bar */}
      <div className="dash-topbar">
        <div>
          <h1>Command console</h1>
          <p className="claim">
            Manage your sovereign identity, personal <code>vk_live_</code> API keys, multi-modal
            playground ({models.length} verified chat models, Flux image synthesis, deep web
            search), synced CLI sessions, and 7-role cognitive routing table.
          </p>
        </div>
        <div className="dash-topbar-actions">
          <span className="dash-status-pill">
            <span className="bh-pulse-dot" />
            <span>
              5 Gateways Online · {models.length} Models · {activeApiKeys.length} Active Keys
            </span>
          </span>
          <button
            type="button"
            className="btn-quiet"
            onClick={handleRefreshTelemetry}
            disabled={refreshing}
          >
            {refreshing ? "Syncing..." : `Refresh telemetry (${lastRefreshed})`}
          </button>
        </div>
      </div>

      {/* 4-Column Live Quota & Telemetry KPI Cards */}
      <section aria-label="Live telemetry counters">
        {usage ? (
          <div className="dash-kpi-grid">
            <div className="meter-block">
              <div className="meter-head">
                <span className="meter-label">Daily chat completions</span>
                <span className="tag">{chatPct}% used</span>
              </div>
              <div className="dash-kpi-number">
                <span>{chatUsed}</span>
                <span className="dash-kpi-denom">/ {chatLimit}</span>
              </div>
              <progress
                className="meter"
                aria-label="Chat completions usage"
                value={Math.min(chatUsed, chatLimit)}
                max={Math.max(chatLimit, 1)}
              />
              <p className="meta">
                <strong>{Math.max(chatLimit - chatUsed, 0)} messages remaining</strong> in UTC
                bucket {usage.today.day}
              </p>
            </div>

            <div className="meter-block">
              <div className="meter-head">
                <span className="meter-label">Daily web search</span>
                <span className="tag">{searchPct}% used</span>
              </div>
              <div className="dash-kpi-number">
                <span>{searchUsed}</span>
                <span className="dash-kpi-denom">/ {searchLimit}</span>
              </div>
              <progress
                className="meter"
                aria-label="Web search usage"
                value={Math.min(searchUsed, searchLimit)}
                max={Math.max(searchLimit, 1)}
              />
              <p className="meta">
                <strong>{Math.max(searchLimit - searchUsed, 0)} searches remaining</strong> across
                Tavily, Brave, Exa &amp; Serper
              </p>
            </div>

            <div className="meter-block">
              <div className="meter-head">
                <span className="meter-label">Minute burst rate</span>
                <span className="tag">{burstPct}% load</span>
              </div>
              <div className="dash-kpi-number">
                <span>{burstUsed}</span>
                <span className="dash-kpi-denom">/ {burstLimit} rpm</span>
              </div>
              <progress
                className="meter"
                aria-label="Burst rate usage"
                value={Math.min(burstUsed, burstLimit)}
                max={Math.max(burstLimit, 1)}
              />
              <p className="meta">
                <strong>{Math.max(burstLimit - burstUsed, 0)} req/min headroom</strong> on sliding
                60s window
              </p>
            </div>

            <div className="meter-block">
              <div className="meter-head">
                <span className="meter-label">Entitled CLI skills</span>
                <span className={`tag ${usage.plan === "pro" ? "tag-pro" : "tag-free"}`}>
                  {usage.plan === "pro" ? "Pro vault" : "Core vault"}
                </span>
              </div>
              <div className="dash-kpi-number">
                <span>{unlockedSkills.length}</span>
                <span className="dash-kpi-denom">/ {skills.length} skills</span>
              </div>
              <progress
                className="meter"
                aria-label="Unlocked skills ratio"
                value={unlockedSkills.length}
                max={Math.max(skills.length, 1)}
              />
              <p className="meta">
                Synced automatically to <code>~/.varynth/skills/</code> on login
              </p>
            </div>
          </div>
        ) : (
          <div className="panel panel-error" role="alert">
            <p className="panel-title">Telemetry counters did not load.</p>
            <p className="meta">{usageError || "Check varynth-api connectivity."}</p>
          </div>
        )}
      </section>

      {/* Identity, Plan & Bearer Token Vault Row */}
      <section className="dash-grid" aria-label="Identity, active tier, and credentials">
        <div className="identity">
          <div style={{ width: "100%" }}>
            <div className="meter-head" style={{ marginBottom: "var(--space-sm)" }}>
              <span className="meter-label">Sovereign account identity</span>
              <span className="tag tag-ok">{me ? "Verified session" : "Offline"}</span>
            </div>
            {me ? (
              <dl className="facts">
                <div>
                  <dt>Email</dt>
                  <dd>{me.email}</dd>
                </div>
                <div>
                  <dt>Account UUID</dt>
                  <dd className="dash-inline-copy">
                    <span className="mono">{me.id}</span>
                    <button
                      type="button"
                      className="copy-cmd-btn"
                      onClick={() => {
                        navigator.clipboard.writeText(me.id);
                        setCopiedId(true);
                        setTimeout(() => setCopiedId(false), 1800);
                      }}
                    >
                      {copiedId ? "Copied" : "Copy ID"}
                    </button>
                  </dd>
                </div>
                <div>
                  <dt>Session JWT</dt>
                  <dd className="dash-inline-copy">
                    <code className="dash-token-preview">{tokenDisplay}</code>
                    <button
                      type="button"
                      className="copy-cmd-btn"
                      onClick={() => setShowToken((prev) => !prev)}
                    >
                      {showToken ? "Hide" : "Reveal"}
                    </button>
                    <button
                      type="button"
                      className="copy-cmd-btn"
                      onClick={() => {
                        navigator.clipboard.writeText(sessionJwt);
                        setCopiedToken(true);
                        setTimeout(() => setCopiedToken(false), 1800);
                      }}
                    >
                      {copiedToken ? "Copied" : "Copy JWT"}
                    </button>
                  </dd>
                </div>
              </dl>
            ) : (
              <p className="meta">{meError || "Identity unavailable."}</p>
            )}
          </div>
          <div className="dash-identity-foot">
            <button
              type="button"
              className="copy-cmd-btn"
              onClick={() => setActiveTab("keys")}
            >
              Manage vk_live_ API Keys ({activeApiKeys.length})
            </button>
            <SignOutButton />
          </div>
        </div>

        <div className="plan-card">
          <div style={{ width: "100%" }}>
            <div className="meter-head" style={{ marginBottom: "var(--space-sm)" }}>
              <span className="meter-label">Subscription &amp; entitlement tier</span>
              <span className={`tag ${usage?.plan === "pro" ? "tag-pro" : "tag-free"}`}>
                {usage?.plan === "pro" ? "Pro unlocked" : "Free tier"}
              </span>
            </div>
            <p className="plan-name" style={{ marginBottom: "var(--space-xs)" }}>
              {usage?.plan === "pro" ? "Varynth Pro" : "Varynth Free"}
            </p>
            <p className="meta" style={{ marginBottom: "var(--space-md)" }}>
              {usage
                ? `${usage.limits.chat_per_day} daily completions · ${usage.limits.chat_per_minute} req/min burst · ${usage.limits.search_per_day} deep web searches/day`
                : "Quota details unavailable"}
            </p>
            <ul className="features" style={{ margin: "0 0 var(--space-md)" }}>
              <li>
                All {models.length} verified chat &amp; reasoning models + Flux image synthesis
                across 5 worker clusters
              </li>
              <li>Personal vk_live_ API keys, multi-agent CLI swarm &amp; cloud session sync</li>
              <li>
                {usage?.plan === "pro"
                  ? "Pro skills unlocked: motion-frames-pro, computer-use, hallmark, impeccable"
                  : "Upgrade to Pro to unlock 60fps Flux video studio & computer-use overlay skills"}
              </li>
            </ul>
          </div>
          <div style={{ width: "100%" }}>
            {usage?.plan === "pro" ? (
              billingConfigured ? (
                <PlanActions
                  currentPlan={usage.plan}
                  target="free"
                  label="Switch to Free tier"
                />
              ) : (
                <p className="provision">
                  Pro tier is active on your account. Self-service billing changes are locked by
                  server policy.
                </p>
              )
            ) : billingConfigured && usage ? (
              <PlanActions
                currentPlan={usage.plan}
                target="pro"
                label="Upgrade to Varynth Pro"
              />
            ) : (
              <div className="dash-inline-copy" style={{ justifyContent: "space-between" }}>
                <span className="meta">
                  Operator-managed billing is active on this gateway node.
                </span>
                <a className="btn-quiet" href="/pricing">
                  Compare plans
                </a>
              </div>
            )}
          </div>
        </div>
      </section>

      {/* Interactive 7-Tab Developer Console */}
      <section className="dash-workspace" aria-label="Interactive developer tools">
        <div className="dash-tab-bar" role="tablist" aria-label="Dashboard sections">
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "playground"}
            className={activeTab === "playground" ? "dash-tab-btn active" : "dash-tab-btn"}
            onClick={() => setActiveTab("playground")}
          >
            <span>Multi-Modal Studio</span>
            <span className="install-tab-badge">Chat · Flux · Search</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "byok"}
            className={activeTab === "byok" ? "dash-tab-btn active" : "dash-tab-btn"}
            onClick={() => setActiveTab("byok")}
          >
            <span>BYOK Vault</span>
            <span className="install-tab-badge" style={{ color: "#38bdf8", borderColor: "rgba(56, 189, 248, 0.3)" }}>Your Keys</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "keys"}
            className={activeTab === "keys" ? "dash-tab-btn active" : "dash-tab-btn"}
            onClick={() => setActiveTab("keys")}
          >
            <span>API Keys</span>
            <span className="install-tab-badge">{activeApiKeys.length} active</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "sessions"}
            className={activeTab === "sessions" ? "dash-tab-btn active" : "dash-tab-btn"}
            onClick={() => setActiveTab("sessions")}
          >
            <span>CLI Sessions &amp; Swarm</span>
            <span className="install-tab-badge">{cliSessions.length} synced</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "router"}
            className={activeTab === "router" ? "dash-tab-btn active" : "dash-tab-btn"}
            onClick={() => setActiveTab("router")}
          >
            <span>7-Role Router</span>
            <span className="install-tab-badge">config.toml</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "skills"}
            className={activeTab === "skills" ? "dash-tab-btn active" : "dash-tab-btn"}
            onClick={() => setActiveTab("skills")}
          >
            <span>Skills Vault</span>
            <span className="install-tab-badge">
              {unlockedSkills.length}/{skills.length}
            </span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "sdk"}
            className={activeTab === "sdk" ? "dash-tab-btn active" : "dash-tab-btn"}
            onClick={() => setActiveTab("sdk")}
          >
            <span>SDK Snippets</span>
            <span className="install-tab-badge">OpenAI API</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "workstation"}
            className={activeTab === "workstation" ? "dash-tab-btn active" : "dash-tab-btn"}
            onClick={() => setActiveTab("workstation")}
          >
            <span>Workstation Setup</span>
            <span className="install-tab-badge">Win / Unix</span>
          </button>
        </div>

        {/* TAB 1: MULTI-MODAL STUDIO (CHAT + FLUX IMAGE + WEB SEARCH) */}
        {activeTab === "playground" && (
          <div className="dash-tab-panel">
            <div className="models-filter-pills" style={{ marginBottom: "var(--space-md)" }}>
              <button
                type="button"
                className={pgMode === "chat" ? "filter-pill active" : "filter-pill"}
                onClick={() => setPgMode("chat")}
              >
                Chat &amp; Reasoning ({models.length} Models)
              </button>
              <button
                type="button"
                className={pgMode === "image" ? "filter-pill active" : "filter-pill"}
                onClick={() => setPgMode("image")}
              >
                Flux Image Synthesis Studio
              </button>
              <button
                type="button"
                className={pgMode === "search" ? "filter-pill active" : "filter-pill"}
                onClick={() => setPgMode("search")}
              >
                Multi-Engine Web Search Tester
              </button>
            </div>

            <div className="dash-pg-grid">
              <form className="dash-pg-form" onSubmit={handleRunPlayground}>
                {pgMode === "chat" && (
                  <>
                    <div className="field">
                      <label htmlFor="pg-model-select">
                        Target gateway model ({models.length} live)
                      </label>
                      <select
                        id="pg-model-select"
                        className="dash-select"
                        value={selectedModel}
                        onChange={(e) => setSelectedModel(e.target.value)}
                      >
                        {models.map((m) => (
                          <option key={m.id} value={m.id}>
                            {m.id} — {m.display_name} ({m.owned_by})
                          </option>
                        ))}
                      </select>
                    </div>

                    <div className="field">
                      <label htmlFor="pg-system-input">System instruction</label>
                      <input
                        id="pg-system-input"
                        type="text"
                        value={systemPrompt}
                        onChange={(e) => setSystemPrompt(e.target.value)}
                      />
                    </div>

                    <div className="field">
                      <div className="meter-head">
                        <label htmlFor="pg-prompt-input">Prompt</label>
                        <div className="dash-prompt-pills">
                          {QUICK_PROMPTS.map((qp) => (
                            <button
                              key={qp.label}
                              type="button"
                              className="copy-cmd-btn"
                              onClick={() => setUserPrompt(qp.prompt)}
                            >
                              {qp.label}
                            </button>
                          ))}
                        </div>
                      </div>
                      <textarea
                        id="pg-prompt-input"
                        className="dash-textarea"
                        rows={4}
                        value={userPrompt}
                        onChange={(e) => setUserPrompt(e.target.value)}
                        placeholder="Enter a coding, architecture, or reasoning prompt..."
                      />
                    </div>
                  </>
                )}

                {pgMode === "image" && (
                  <>
                    <div className="field">
                      <label htmlFor="pg-image-model">Flux synthesis engine</label>
                      <select
                        id="pg-image-model"
                        className="dash-select"
                        value={imageModel}
                        onChange={(e) => setImageModel(e.target.value)}
                      >
                        {IMAGE_MODELS.map((im) => (
                          <option key={im.id} value={im.id}>
                            {im.id} — {im.name}
                          </option>
                        ))}
                      </select>
                    </div>

                    <div className="field">
                      <div className="meter-head">
                        <label htmlFor="pg-image-prompt">Visual synthesis prompt</label>
                        <div className="dash-prompt-pills">
                          {IMAGE_QUICK_PROMPTS.map((qp) => (
                            <button
                              key={qp.label}
                              type="button"
                              className="copy-cmd-btn"
                              onClick={() => setImagePrompt(qp.prompt)}
                            >
                              {qp.label}
                            </button>
                          ))}
                        </div>
                      </div>
                      <textarea
                        id="pg-image-prompt"
                        className="dash-textarea"
                        rows={4}
                        value={imagePrompt}
                        onChange={(e) => setImagePrompt(e.target.value)}
                        placeholder="Describe the hero asset, lighting, lens, and atmosphere..."
                      />
                    </div>
                  </>
                )}

                {pgMode === "search" && (
                  <>
                    <div className="field">
                      <label htmlFor="pg-search-provider">Search engine routing</label>
                      <select
                        id="pg-search-provider"
                        className="dash-select"
                        value={searchProvider}
                        onChange={(e) => setSearchProvider(e.target.value)}
                      >
                        {SEARCH_PROVIDERS.map((sp) => (
                          <option key={sp.id} value={sp.id}>
                            {sp.name}
                          </option>
                        ))}
                      </select>
                    </div>

                    <div className="field">
                      <label htmlFor="pg-search-query">Deep web query</label>
                      <textarea
                        id="pg-search-query"
                        className="dash-textarea"
                        rows={3}
                        value={searchQuery}
                        onChange={(e) => setSearchQuery(e.target.value)}
                        placeholder="Search technical documentation, CVEs, or crates..."
                      />
                    </div>
                  </>
                )}

                <div className="dash-pg-actions">
                  <button type="submit" className="btn" disabled={pgRunning}>
                    {pgRunning
                      ? "Dispatching to cluster..."
                      : pgMode === "image"
                        ? `Synthesize with ${imageModel}`
                        : pgMode === "search"
                          ? `Search via ${searchProvider}`
                          : `Run on ${selectedModel}`}
                  </button>
                  <span className="meta">
                    {pgMode === "image"
                      ? "Executes POST /v1/images/generations across 5 worker gateways."
                      : pgMode === "search"
                        ? "Executes POST /v1/search with automatic multi-engine failover."
                        : "Executes POST /v1/chat/completions with 5-gateway failover."}
                  </span>
                </div>
              </form>

              <div className="dash-pg-output-card">
                <div
                  className="meter-head"
                  style={{
                    paddingBottom: "var(--space-sm)",
                    borderBottom: "var(--rule-hair) solid var(--color-rule)",
                  }}
                >
                  <span className="meter-label">
                    {pgMode === "image"
                      ? "Flux studio canvas"
                      : pgMode === "search"
                        ? "Live citation stream"
                        : "Cluster response stream"}
                  </span>
                  <div style={{ display: "flex", gap: "var(--space-xs)", alignItems: "center" }}>
                    {pgLatency !== null && <span className="tag tag-ok">{pgLatency} ms</span>}
                    <span className="mono">
                      {pgMode === "image"
                        ? imageModel
                        : pgMode === "search"
                          ? `search/${searchProvider}`
                          : selectedModel}
                    </span>
                  </div>
                </div>
                <div className="dash-pg-response-body">
                  {pgRunning ? (
                    <p className="meta">Awaiting cluster synthesis...</p>
                  ) : pgError ? (
                    <div className="error-banner" role="alert">
                      {pgError}
                    </div>
                  ) : pgImageSrc ? (
                    <div style={{ display: "flex", flexDirection: "column", gap: "var(--space-sm)" }}>
                      <img
                        src={pgImageSrc}
                        alt={imagePrompt}
                        style={{
                          width: "100%",
                          maxHeight: "440px",
                          objectFit: "contain",
                          borderRadius: "var(--radius-md)",
                          border: "var(--rule-hair) solid var(--color-rule)",
                        }}
                      />
                      <div
                        style={{
                          display: "flex",
                          justifyContent: "space-between",
                          alignItems: "center",
                          gap: "var(--space-xs)",
                          flexWrap: "wrap",
                        }}
                      >
                        <span className="meta">Synthesized by {imageModel}</span>
                        <div style={{ display: "flex", gap: "var(--space-xs)", alignItems: "center" }}>
                          <button
                            type="button"
                            className="copy-cmd-btn"
                            onClick={() => {
                              navigator.clipboard.writeText(pgImageSrc);
                              setCopiedImgB64(true);
                              setTimeout(() => setCopiedImgB64(false), 1800);
                            }}
                          >
                            {copiedImgB64 ? "Copied Base64!" : "Copy Base64"}
                          </button>
                          <button
                            type="button"
                            className="btn"
                            onClick={handleDownloadImage}
                          >
                            Download Image
                          </button>
                        </div>
                      </div>
                    </div>
                  ) : pgSearchResults.length > 0 ? (
                    <div style={{ display: "flex", flexDirection: "column", gap: "var(--space-sm)" }}>
                      {pgSearchResults.map((item, idx) => (
                        <div key={idx} className="dash-role-row" style={{ flexDirection: "column", alignItems: "flex-start" }}>
                          <a
                            href={item.url || "#"}
                            target="_blank"
                            rel="noreferrer"
                            style={{ fontWeight: 600, color: "var(--color-accent)" }}
                          >
                            {item.title || item.url || `Result #${idx + 1}`}
                          </a>
                          {item.url && <span className="mono" style={{ fontSize: "0.75rem" }}>{item.url}</span>}
                          <p className="meta" style={{ marginTop: "var(--space-2xs)" }}>
                            {item.content || item.snippet || ""}
                          </p>
                        </div>
                      ))}
                    </div>
                  ) : pgOutput ? (
                    <pre className="dash-code-pre">{pgOutput}</pre>
                  ) : (
                    <p className="meta">
                      Select a mode above (<strong>Chat</strong>, <strong>Flux Image Studio</strong>,
                      or <strong>Web Search</strong>) and click the action button to run live
                      inference from your browser.
                    </p>
                  )}
                </div>
              </div>
            </div>
          </div>
        )}

        {/* TAB: BYOK KEYS VAULT */}
        {activeTab === "byok" && (
          <div className="dash-tab-panel">
            <ByokVault />
          </div>
        )}

        {/* TAB 2: PERSONAL API KEYS (vk_live_...) */}
        {activeTab === "keys" && (
          <div className="dash-tab-panel">
            <div className="dash-pg-grid">
              <div className="dash-pg-form">
                <div>
                  <h3 style={{ marginBottom: "var(--space-2xs)" }}>
                    Mint personal API key (<code>vk_live_...</code>)
                  </h3>
                  <p className="meta">
                    Personal API keys authenticate with <code>Authorization: Bearer vk_live_...</code>{" "}
                    across all Varynth endpoints (<code>/v1/chat/completions</code>,{" "}
                    <code>/v1/images/generations</code>, <code>/v1/search</code>, and{" "}
                    <code>varynth</code> CLI) without expiring when your browser session rotates.
                  </p>
                </div>

                <form onSubmit={handleCreateApiKey} style={{ display: "flex", flexDirection: "column", gap: "var(--space-sm)" }}>
                  <div className="field">
                    <label htmlFor="new-key-name">Key label / workstation name</label>
                    <input
                      id="new-key-name"
                      type="text"
                      value={newKeyName}
                      onChange={(e) => setNewKeyName(e.target.value)}
                      placeholder="e.g. MacBook Pro CLI, GitHub Actions Review, Cursor IDE"
                      required
                    />
                  </div>
                  <div className="dash-pg-actions">
                    <button type="submit" className="btn" disabled={mintingKey}>
                      {mintingKey ? "Generating vk_live_ key..." : "Create secret API key"}
                    </button>
                  </div>
                </form>

                {keyError && (
                  <div className="error-banner" role="alert">
                    {keyError}
                  </div>
                )}

                {mintedSecretKey && (
                  <div className="meter-block" style={{ borderColor: "var(--color-accent)" }}>
                    <div className="meter-head">
                      <span className="meter-label">New secret key created — copy now</span>
                      <span className="tag tag-pro">Shown once</span>
                    </div>
                    <div className="dash-inline-copy" style={{ marginTop: "var(--space-xs)" }}>
                      <code className="dash-token-preview" style={{ maxWidth: "100%" }}>
                        {mintedSecretKey}
                      </code>
                      <button
                        type="button"
                        className="copy-cmd-btn"
                        onClick={() => {
                          navigator.clipboard.writeText(mintedSecretKey);
                          setCopiedSecretKey(true);
                          setTimeout(() => setCopiedSecretKey(false), 2000);
                        }}
                      >
                        {copiedSecretKey ? "Copied!" : "Copy vk_live_ key"}
                      </button>
                    </div>
                    <p className="meta" style={{ marginTop: "var(--space-xs)" }}>
                      Store this key in <code>export VARYNTH_API_KEY=&quot;{mintedSecretKey}&quot;</code>.
                      For security, only the SHA-256 hash is stored in PostgreSQL.
                    </p>
                  </div>
                )}
              </div>

              <div className="dash-pg-output-card">
                <div
                  className="meter-head"
                  style={{
                    paddingBottom: "var(--space-sm)",
                    borderBottom: "var(--rule-hair) solid var(--color-rule)",
                  }}
                >
                  <span className="meter-label">Issued API keys ({apiKeys.length})</span>
                  <span className="tag">{activeApiKeys.length} active</span>
                </div>

                <div className="dash-roles-list" style={{ marginTop: "var(--space-sm)" }}>
                  {apiKeys.length === 0 ? (
                    <p className="meta">
                      No personal API keys minted yet. Create one on the left to use Varynth in
                      CI/CD pipelines, Cursor/Continue IDEs, or OpenAI SDK scripts.
                    </p>
                  ) : (
                    apiKeys.map((k) => (
                      <div key={k.id} className="dash-role-row">
                        <div className="dash-role-meta">
                          <div style={{ display: "flex", alignItems: "center", gap: "var(--space-xs)" }}>
                            <strong>{k.name}</strong>
                            <code>{k.key_prefix}</code>
                            <span className={`tag ${k.revoked ? "tag-locked" : "tag-ok"}`}>
                              {k.revoked ? "Revoked" : "Active"}
                            </span>
                          </div>
                          <p className="meta">
                            Created {new Date(k.created_at).toISOString().slice(0, 10)} ·{" "}
                            {k.last_used_at
                              ? `Last used ${new Date(k.last_used_at).toISOString().slice(0, 16).replace("T", " ")} UTC`
                              : "Never used"}
                          </p>
                        </div>
                        {!k.revoked && (
                          <button
                            type="button"
                            className="copy-cmd-btn"
                            onClick={() => handleRevokeApiKey(k.id)}
                          >
                            Revoke
                          </button>
                        )}
                      </div>
                    ))
                  )}
                </div>
              </div>
            </div>
          </div>
        )}

        {/* TAB 3: CLI SESSIONS & MULTI-AGENT SWARM SYNC */}
        {activeTab === "sessions" && (
          <div className="dash-tab-panel">
            <div className="dash-pg-grid">
              <div className="dash-pg-form">
                <div>
                  <h3 style={{ marginBottom: "var(--space-2xs)" }}>
                    Cloud CLI session sync &amp; multi-agent swarm
                  </h3>
                  <p className="meta">
                    Synchronize local <code>.varynth/sessions/*.jsonl</code> transcripts to your
                    cloud console with <code>varynth sync</code>, run parallel 3-agent swarms with{" "}
                    <code>varynth swarm</code>, or audit Git diffs with <code>varynth review</code>.
                  </p>
                </div>

                <div className="instrument-panel">
                  <div className="instrument-head">
                    <span>Varynth CLI v0.1.3 — Swarm &amp; Sync Commands</span>
                    <span className="instrument-status">3-Agent Parallel</span>
                  </div>
                  <div className="instrument-body">
                    <div className="term-line">
                      <span className="term-cool">$</span>
                      <span className="term-cmd">
                        varynth swarm &quot;Refactor auth middleware for zero-copy JWT&quot;
                      </span>
                    </div>
                    <div className="term-line">
                      <span className="term-accent">review</span>
                      <span className="term-cmd">varynth review --staged</span>
                    </div>
                    <div className="term-line">
                      <span className="term-accent">cloud</span>
                      <span className="term-ok">varynth sync</span>
                    </div>
                  </div>
                </div>

                <div className="dash-pg-actions">
                  <button
                    type="button"
                    className="btn"
                    onClick={handleSyncDemoSession}
                    disabled={syncingSession}
                  >
                    {syncingSession ? "Syncing session..." : "Simulate CLI session sync"}
                  </button>
                  <span className="meta">
                    Tests <code>POST /v1/me/sessions</code> live and adds a sample swarm trace.
                  </span>
                </div>
              </div>

              <div className="dash-pg-output-card">
                <div
                  className="meter-head"
                  style={{
                    paddingBottom: "var(--space-sm)",
                    borderBottom: "var(--rule-hair) solid var(--color-rule)",
                  }}
                >
                  <span className="meter-label">
                    Synced CLI &amp; Swarm sessions ({cliSessions.length})
                  </span>
                  <span className="tag tag-ok">Cloud Vault</span>
                </div>

                <div className="dash-roles-list" style={{ marginTop: "var(--space-sm)" }}>
                  {cliSessions.length === 0 ? (
                    <p className="meta">
                      No CLI sessions synced yet. Run <code>varynth sync</code> in your terminal or
                      click <strong>Simulate CLI session sync</strong> on the left to test.
                    </p>
                  ) : (
                    cliSessions.map((sess) => (
                      <div
                        key={sess.session_key}
                        className="dash-role-row"
                        style={{ flexDirection: "column", alignItems: "stretch" }}
                      >
                        <div className="meter-head">
                          <div style={{ display: "flex", alignItems: "center", gap: "var(--space-xs)" }}>
                            <strong>{sess.title}</strong>
                            <code>{sess.session_key}</code>
                          </div>
                          <div style={{ display: "flex", gap: "var(--space-2xs)" }}>
                            <span className="tag">{sess.message_count} turns</span>
                            <button
                              type="button"
                              className="copy-cmd-btn"
                              onClick={() => {
                                const cmd = `varynth --continue ${sess.session_key}`;
                                navigator.clipboard.writeText(cmd);
                                setCopiedSessionCmd(sess.session_key);
                                setTimeout(() => setCopiedSessionCmd(null), 1800);
                              }}
                            >
                              {copiedSessionCmd === sess.session_key ? "Copied!" : "Copy resume"}
                            </button>
                            <button
                              type="button"
                              className="copy-cmd-btn"
                              onClick={() => handleDeleteSession(sess.session_key)}
                            >
                              Delete
                            </button>
                          </div>
                        </div>
                        <p className="meta" style={{ marginTop: "var(--space-2xs)" }}>
                          {sess.preview}
                        </p>
                        <div
                          className="meta"
                          style={{
                            display: "flex",
                            justifyContent: "space-between",
                            marginTop: "var(--space-2xs)",
                            fontSize: "0.75rem",
                          }}
                        >
                          <span>
                            Model: <code>{sess.model}</code> · Workspace: <code>{sess.workspace}</code>
                          </span>
                          <span>
                            {new Date(sess.updated_at).toISOString().slice(0, 16).replace("T", " ")} UTC
                          </span>
                        </div>
                      </div>
                    ))
                  )}
                </div>
              </div>
            </div>
          </div>
        )}

        {/* TAB 4: 7-ROLE COGNITIVE ROUTER */}
        {activeTab === "router" && (
          <div className="dash-tab-panel">
            <div className="dash-pg-grid">
              <div className="dash-roles-list">
                {ROLE_DEFS.map((r) => (
                  <div key={r.role} className="dash-role-row">
                    <div className="dash-role-meta">
                      <div style={{ display: "flex", alignItems: "center", gap: "var(--space-xs)" }}>
                        <span className="tag">{r.hotkey}</span>
                        <strong>{r.title}</strong>
                        <code>roles.{r.role}</code>
                      </div>
                      <p className="meta">{r.desc}</p>
                    </div>
                    <select
                      aria-label={`Model for ${r.role} role`}
                      className="dash-select"
                      value={roleMap[r.role] || r.defaultModel}
                      onChange={(e) =>
                        setRoleMap((prev) => ({ ...prev, [r.role]: e.target.value }))
                      }
                    >
                      {models.map((m) => (
                        <option key={m.id} value={m.id}>
                          {m.id} ({m.display_name})
                        </option>
                      ))}
                    </select>
                  </div>
                ))}
              </div>

              <div className="dash-pg-output-card">
                <div
                  className="meter-head"
                  style={{
                    paddingBottom: "var(--space-sm)",
                    borderBottom: "var(--rule-hair) solid var(--color-rule)",
                  }}
                >
                  <span className="meter-label">Generated ~/.varynth/config.toml</span>
                  <div style={{ display: "flex", gap: "var(--space-xs)" }}>
                    <button
                      type="button"
                      className="copy-cmd-btn"
                      onClick={() => {
                        navigator.clipboard.writeText(generatedToml);
                        setCopiedToml(true);
                        setTimeout(() => setCopiedToml(false), 1800);
                      }}
                    >
                      {copiedToml ? "Copied" : "Copy TOML"}
                    </button>
                    <button
                      type="button"
                      className="copy-cmd-btn"
                      onClick={handleDownloadToml}
                    >
                      Download config.toml
                    </button>
                  </div>
                </div>
                <pre className="dash-code-pre">{generatedToml}</pre>
                <p className="meta" style={{ marginTop: "var(--space-sm)" }}>
                  Place this file at <code>~/.varynth/config.toml</code> (or{" "}
                  <code>%USERPROFILE%\.varynth\config.toml</code> on Windows) or switch bindings
                  live inside the TUI with <code>/model</code>.
                </p>
              </div>
            </div>
          </div>
        )}

        {/* TAB 5: SKILLS VAULT INSPECTOR */}
        {activeTab === "skills" && (
          <div className="dash-tab-panel">
            <div className="dash-pg-grid">
              <div className="dash-skills-list">
                {skills.map((sk) => {
                  const isSelected = selectedSkill?.id === sk.id;
                  return (
                    <button
                      key={sk.id}
                      type="button"
                      className={isSelected ? "dash-skill-item active" : "dash-skill-item"}
                      onClick={() => setSelectedSkill(sk)}
                    >
                      <div className="meter-head" style={{ width: "100%" }}>
                        <strong>{sk.name}</strong>
                        <div style={{ display: "flex", gap: "var(--space-2xs)" }}>
                          <span className={`tag ${sk.tier === "pro" ? "tag-pro" : "tag-free"}`}>
                            {sk.tier}
                          </span>
                          <span className={`tag ${sk.unlocked ? "tag-ok" : "tag-locked"}`}>
                            {sk.unlocked ? "Synced" : "Locked"}
                          </span>
                        </div>
                      </div>
                      <p className="meta" style={{ textAlign: "left" }}>
                        {sk.description}
                      </p>
                    </button>
                  );
                })}
              </div>

              <div className="dash-pg-output-card">
                {selectedSkill ? (
                  <>
                    <div
                      className="meter-head"
                      style={{
                        paddingBottom: "var(--space-sm)",
                        borderBottom: "var(--rule-hair) solid var(--color-rule)",
                      }}
                    >
                      <div>
                        <span className="meter-label">
                          ~/.varynth/skills/{selectedSkill.id}/SKILL.md
                        </span>
                      </div>
                      {selectedSkill.content && (
                        <button
                          type="button"
                          className="copy-cmd-btn"
                          onClick={() => {
                            navigator.clipboard.writeText(selectedSkill.content || "");
                            setCopiedSkill(true);
                            setTimeout(() => setCopiedSkill(false), 1800);
                          }}
                        >
                          {copiedSkill ? "Copied SKILL.md" : "Copy SKILL.md"}
                        </button>
                      )}
                    </div>
                    {selectedSkill.unlocked && selectedSkill.content ? (
                      <pre className="dash-code-pre">{selectedSkill.content}</pre>
                    ) : (
                      <div style={{ padding: "var(--space-lg) 0" }}>
                        <p className="panel-title" style={{ marginBottom: "var(--space-xs)" }}>
                          Pro entitlement required for {selectedSkill.name}
                        </p>
                        <p className="meta" style={{ marginBottom: "var(--space-md)" }}>
                          Upgrade your account to Varynth Pro to unlock the full{" "}
                          <code>SKILL.md</code> execution manifest for{" "}
                          <code>{selectedSkill.id}</code> and sync it automatically on{" "}
                          <code>varynth login</code>.
                        </p>
                        <a className="btn" href="/pricing">
                          View Pro entitlement details
                        </a>
                      </div>
                    )}
                  </>
                ) : (
                  <p className="meta">Select a skill on the left to inspect its SKILL.md manifest.</p>
                )}
              </div>
            </div>
          </div>
        )}

        {/* TAB 6: API & SDK INTEGRATION */}
        {activeTab === "sdk" && (
          <div className="dash-tab-panel">
            <div className="dash-sdk-card">
              <div className="meter-head" style={{ marginBottom: "var(--space-md)" }}>
                <div className="models-filter-pills">
                  <button
                    type="button"
                    className={sdkLang === "curl" ? "filter-pill active" : "filter-pill"}
                    onClick={() => setSdkLang("curl")}
                  >
                    cURL Chat
                  </button>
                  <button
                    type="button"
                    className={sdkLang === "ts" ? "filter-pill active" : "filter-pill"}
                    onClick={() => setSdkLang("ts")}
                  >
                    TypeScript (OpenAI SDK)
                  </button>
                  <button
                    type="button"
                    className={sdkLang === "python" ? "filter-pill active" : "filter-pill"}
                    onClick={() => setSdkLang("python")}
                  >
                    Python (OpenAI SDK)
                  </button>
                  <button
                    type="button"
                    className={sdkLang === "flux" ? "filter-pill active" : "filter-pill"}
                    onClick={() => setSdkLang("flux")}
                  >
                    Flux Image Synthesis
                  </button>
                  <button
                    type="button"
                    className={sdkLang === "search" ? "filter-pill active" : "filter-pill"}
                    onClick={() => setSdkLang("search")}
                  >
                    Multi-Engine Web Search
                  </button>
                </div>

                <button
                  type="button"
                  className="copy-cmd-btn"
                  onClick={() => {
                    navigator.clipboard.writeText(getSdkSnippet());
                    setCopiedSdk(true);
                    setTimeout(() => setCopiedSdk(false), 1800);
                  }}
                >
                  {copiedSdk ? "Copied snippet" : "Copy code"}
                </button>
              </div>
              <pre className="dash-code-pre">{getSdkSnippet()}</pre>
            </div>
          </div>
        )}

        {/* TAB 7: WORKSTATION SETUP */}
        {activeTab === "workstation" && (
          <div className="dash-tab-panel">
            <InstallTabs />
          </div>
        )}
      </section>
    </div>
  );
}
