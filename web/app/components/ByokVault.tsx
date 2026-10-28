"use client";

import React, { useState, useEffect } from "react";

export interface ByokProviderConfig {
  id: string;
  name: string;
  placeholder: string;
  docsUrl: string;
  protocol: "openai" | "anthropic" | "gemini";
  models: string[];
}

export const BYOK_PROVIDERS: ByokProviderConfig[] = [
  {
    id: "openai",
    name: "OpenAI",
    placeholder: "sk-proj-... / sk-...",
    docsUrl: "https://platform.openai.com/api-keys",
    protocol: "openai",
    models: ["gpt-4o", "gpt-4o-mini", "o1", "o1-preview", "o1-mini", "gpt-4-turbo", "gpt-3.5-turbo"],
  },
  {
    id: "anthropic",
    name: "Anthropic Claude",
    placeholder: "sk-ant-...",
    docsUrl: "https://console.anthropic.com/settings/keys",
    protocol: "anthropic",
    models: ["claude-3-5-sonnet-20241022", "claude-3-5-sonnet-latest", "claude-3-5-haiku-20241022", "claude-3-5-haiku-latest", "claude-3-opus-20240229"],
  },
  {
    id: "openrouter",
    name: "OpenRouter",
    placeholder: "sk-or-v1-...",
    docsUrl: "https://openrouter.ai/keys",
    protocol: "openai",
    models: ["anthropic/claude-3.5-sonnet", "meta-llama/llama-3.3-70b-instruct", "google/gemini-2.0-flash-exp:free", "deepseek/deepseek-r1"],
  },
  {
    id: "gemini",
    name: "Google Gemini",
    placeholder: "AIzaSy...",
    docsUrl: "https://aistudio.google.com/app/apikey",
    protocol: "gemini",
    models: ["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash", "gemini-2.0-flash-exp", "gemini-1.5-pro", "gemini-1.5-flash"],
  },
  {
    id: "groq",
    name: "Groq LPU",
    placeholder: "gsk_...",
    docsUrl: "https://console.groq.com/keys",
    protocol: "openai",
    models: ["llama-3.3-70b-versatile", "mixtral-8x7b-32768", "deepseek-r1-distill-llama-70b"],
  },
  {
    id: "deepseek",
    name: "DeepSeek",
    placeholder: "sk-...",
    docsUrl: "https://platform.deepseek.com/api_keys",
    protocol: "openai",
    models: ["deepseek-chat", "deepseek-coder", "deepseek-reasoner"],
  },
];

export interface ServerByokProvider {
  id: string;
  provider_id: string;
  name: string;
  has_key: boolean;
  key_masked: string;
  base_url: string;
  models: string[];
  models_raw: string;
  protocol: string;
  enabled: boolean;
  created_at?: string;
  updated_at?: string;
}

const LOCAL_STORAGE_KEY = "varynth_byok_keys_vault";
const ACTIVE_PROVIDER_KEY = "varynth_active_provider";

export function ByokVault() {
  const [serverProviders, setServerProviders] = useState<ServerByokProvider[]>([]);
  const [keys, setKeys] = useState<Record<string, string>>({});
  const [activeProvider, setActiveProvider] = useState<string>("managed");
  const [inputValues, setInputValues] = useState<Record<string, string>>({});
  const [statusMessage, setStatusMessage] = useState<string | null>(null);
  const [statusType, setStatusType] = useState<"success" | "error" | "info">("info");
  const [visibleKeys, setVisibleKeys] = useState<Record<string, boolean>>({});

  // OmniRoute-style Custom API form state
  const [showCustomForm, setShowCustomForm] = useState<boolean>(false);
  const [customName, setCustomName] = useState<string>("");
  const [customBaseUrl, setCustomBaseUrl] = useState<string>("");
  const [customApiKey, setCustomApiKey] = useState<string>("");
  const [customProtocol, setCustomProtocol] = useState<"openai" | "anthropic">("openai");
  const [customModels, setCustomModels] = useState<string>("");
  const [customTesting, setCustomTesting] = useState<boolean>(false);
  const [customTestResult, setCustomTestResult] = useState<{ ok: boolean; message: string } | null>(null);

  // Load providers from API
  async function loadServerProviders() {
    try {
      const res = await fetch("/api/byok", { cache: "no-store" });
      if (res.ok) {
        const data = await res.json();
        const list: ServerByokProvider[] = data.providers || [];
        setServerProviders(list);

        const newKeys: Record<string, string> = {};
        const newInputs: Record<string, string> = {};
        list.forEach((p) => {
          if (p.has_key) {
            newKeys[p.provider_id] = p.key_masked || "••••••••";
            newInputs[p.provider_id] = p.key_masked || "••••••••";
          }
        });
        setKeys(newKeys);
        setInputValues(newInputs);
      }
    } catch (_) {
      // Fallback to local storage if API call fails
      try {
        const stored = localStorage.getItem(LOCAL_STORAGE_KEY);
        if (stored) {
          const parsed = JSON.parse(stored);
          setKeys(parsed);
          setInputValues(parsed);
        }
      } catch (_) {}
    }
  }

  useEffect(() => {
    loadServerProviders();
    try {
      const act = localStorage.getItem(ACTIVE_PROVIDER_KEY);
      if (act) setActiveProvider(act);
    } catch (_) {}
  }, []);

  function notify(msg: string, type: "success" | "error" | "info" = "info", duration = 4000) {
    setStatusMessage(msg);
    setStatusType(type);
    setTimeout(() => setStatusMessage(null), duration);
  }

  async function saveStandardKey(provider: ByokProviderConfig) {
    const val = (inputValues[provider.id] || "").trim();
    if (!val || val.startsWith("••••")) {
      notify("Please enter a valid API key.", "error");
      return;
    }

    try {
      const res = await fetch("/api/byok", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          provider_id: provider.id,
          name: provider.name,
          api_key: val,
          protocol: provider.protocol,
          models: provider.models.join(", "),
          enabled: true,
        }),
      });

      if (res.ok) {
        notify(`✅ ${provider.name} key saved to API and activated for all /v1/models calls!`, "success");
        await loadServerProviders();
        try {
          const updated = { ...keys, [provider.id]: val };
          localStorage.setItem(LOCAL_STORAGE_KEY, JSON.stringify(updated));
        } catch (_) {}
      } else {
        const err = await res.json().catch(() => ({}));
        notify(`Error saving key: ${err.error?.message || "Failed"}`, "error");
      }
    } catch (e: unknown) {
      notify(`Network error: ${e instanceof Error ? e.message : "Failed"}`, "error");
    }
  }

  async function clearKey(providerId: string, name: string) {
    try {
      const res = await fetch("/api/byok", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          action: "delete",
          provider_id: providerId,
        }),
      });

      if (res.ok) {
        notify(`🗑️ ${name} key removed.`, "info");
        await loadServerProviders();
        const updated = { ...keys };
        delete updated[providerId];
        setKeys(updated);
        setInputValues({ ...inputValues, [providerId]: "" });
        try {
          localStorage.setItem(LOCAL_STORAGE_KEY, JSON.stringify(updated));
        } catch (_) {}
      }
    } catch (_) {
      notify("Could not clear key from server.", "error");
    }
  }

  async function testAndFetchCustomModels() {
    if (!customBaseUrl.trim()) {
      setCustomTestResult({ ok: false, message: "Base URL is required." });
      return;
    }
    setCustomTesting(true);
    setCustomTestResult(null);
    try {
      const res = await fetch("/api/byok/test", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          base_url: customBaseUrl.trim(),
          api_key: customApiKey.trim(),
        }),
      });
      const data = await res.json().catch(() => ({}));
      if (data.ok && Array.isArray(data.models) && data.models.length > 0) {
        setCustomTestResult({ ok: true, message: data.message });
        if (!customModels.trim()) {
          setCustomModels(data.models.join(", "));
        }
      } else {
        setCustomTestResult({
          ok: data.ok || false,
          message: data.message || "Endpoint reachable, please enter model names manually.",
        });
      }
    } catch (err: unknown) {
      setCustomTestResult({
        ok: false,
        message: err instanceof Error ? err.message : "Connection failed",
      });
    } finally {
      setCustomTesting(false);
    }
  }

  async function saveCustomProvider() {
    if (!customName.trim() || !customBaseUrl.trim()) {
      notify("Provider Name and Base URL are required.", "error");
      return;
    }
    if (!customModels.trim()) {
      notify("Please provide at least one model name for this custom provider.", "error");
      return;
    }

    const slug = "custom_" + customName.toLowerCase().replace(/[^a-z0-9]/g, "_").slice(0, 24) + "_" + Math.random().toString(36).slice(2, 6);

    try {
      const res = await fetch("/api/byok", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          provider_id: slug,
          name: customName.trim(),
          base_url: customBaseUrl.trim(),
          api_key: customApiKey.trim(),
          protocol: customProtocol,
          models: customModels.trim(),
          enabled: true,
        }),
      });

      if (res.ok) {
        notify(`🎉 Custom API "${customName}" added successfully! Models are now live on /v1/models.`, "success");
        setCustomName("");
        setCustomBaseUrl("");
        setCustomApiKey("");
        setCustomModels("");
        setCustomTestResult(null);
        setShowCustomForm(false);
        await loadServerProviders();
      } else {
        const err = await res.json().catch(() => ({}));
        notify(`Error adding custom API: ${err.error?.message || "Failed"}`, "error");
      }
    } catch (e: unknown) {
      notify(`Network error: ${e instanceof Error ? e.message : "Failed"}`, "error");
    }
  }

  function toggleActiveEngine(providerId: string) {
    setActiveProvider(providerId);
    try {
      localStorage.setItem(ACTIVE_PROVIDER_KEY, providerId);
      notify(`🎯 Active engine switched to: ${providerId === "managed" ? "Varynth Cloud (21 Models)" : providerId.toUpperCase()}`, "info");
    } catch (_) {}
  }

  // Filter custom providers from server
  const customProviders = serverProviders.filter((p) => p.provider_id.startsWith("custom_"));
  const configuredCount = serverProviders.filter((p) => p.has_key).length;
  const totalByokModels = serverProviders
    .filter((p) => p.has_key)
    .reduce((acc, p) => acc + (p.models?.length || 0), 0);

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: "24px" }}>
      {/* Header Banner */}
      <div
        style={{
          background: "linear-gradient(135deg, rgba(15, 23, 42, 0.85) 0%, rgba(30, 41, 59, 0.7) 100%)",
          border: "1px solid rgba(56, 189, 248, 0.25)",
          borderRadius: "12px",
          padding: "24px",
          backdropFilter: "blur(16px)",
          display: "flex",
          flexDirection: "column",
          gap: "14px",
        }}
      >
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "flex-start", flexWrap: "wrap", gap: "12px" }}>
          <div>
            <div style={{ display: "inline-flex", alignItems: "center", gap: "8px", color: "#38bdf8", fontSize: "12px", fontWeight: 700, letterSpacing: "0.1em", textTransform: "uppercase" }}>
              <span style={{ width: "8px", height: "8px", borderRadius: "50%", background: "#38bdf8", boxShadow: "0 0 10px #38bdf8" }} />
              BYOK Client & Server Vault (OmniRoute Enabled)
            </div>
            <h2 style={{ fontSize: "1.6rem", fontWeight: 800, color: "#f8fafc", margin: "6px 0 4px 0", letterSpacing: "-0.02em" }}>
              Bring Your Own Key & Custom APIs
            </h2>
            <p style={{ color: "#94a3b8", fontSize: "14px", margin: 0, maxWidth: "720px", lineHeight: 1.5 }}>
              Attach your personal API keys (OpenAI, Anthropic, Gemini, DeepSeek, OpenRouter) or add custom endpoints (vLLM, Ollama, Together AI). All attached models are dynamically injected into <code>/v1/models</code> and served with zero rate limits!
            </p>
          </div>
          <div style={{ display: "flex", flexDirection: "column", alignItems: "flex-end", gap: "6px" }}>
            <div style={{ display: "flex", alignItems: "center", gap: "10px", background: "rgba(3, 7, 18, 0.6)", padding: "8px 14px", borderRadius: "8px", border: "1px solid rgba(255, 255, 255, 0.08)" }}>
              <span style={{ fontSize: "12px", color: "#cbd5e1" }}>Active Vault:</span>
              <span style={{ fontSize: "12px", fontWeight: 700, color: configuredCount > 0 ? "#10b981" : "#f59e0b" }}>
                {configuredCount} Keys / Endpoints Active
              </span>
            </div>
            <span style={{ fontSize: "11px", color: "#64748b" }}>
              {21 + totalByokModels} total models live in <code>/v1/models</code>
            </span>
          </div>
        </div>

        {statusMessage && (
          <div
            style={{
              padding: "10px 14px",
              borderRadius: "6px",
              background: statusType === "success" ? "rgba(16, 185, 129, 0.15)" : statusType === "error" ? "rgba(239, 68, 68, 0.15)" : "rgba(56, 189, 248, 0.15)",
              border: `1px solid ${statusType === "success" ? "rgba(16, 185, 129, 0.4)" : statusType === "error" ? "rgba(239, 68, 68, 0.4)" : "rgba(56, 189, 248, 0.3)"}`,
              color: "#f8fafc",
              fontSize: "13px",
              fontWeight: 500,
            }}
          >
            {statusMessage}
          </div>
        )}
      </div>

      {/* Engine Selection Bar */}
      <div style={{ display: "flex", gap: "10px", flexWrap: "wrap", alignItems: "center" }}>
        <button
          type="button"
          onClick={() => toggleActiveEngine("managed")}
          style={{
            padding: "8px 16px",
            borderRadius: "8px",
            fontSize: "13px",
            fontWeight: 600,
            cursor: "pointer",
            border: activeProvider === "managed" ? "1px solid #10b981" : "1px solid rgba(255, 255, 255, 0.1)",
            background: activeProvider === "managed" ? "rgba(16, 185, 129, 0.15)" : "rgba(15, 23, 42, 0.6)",
            color: activeProvider === "managed" ? "#10b981" : "#94a3b8",
            transition: "all 0.15s",
          }}
        >
          ● Varynth Managed Cloud (21 Models)
        </button>

        {BYOK_PROVIDERS.map((p) => {
          const srv = serverProviders.find((sp) => sp.provider_id === p.id);
          const hasKey = !!srv?.has_key || !!keys[p.id];
          return (
            <button
              key={p.id}
              type="button"
              disabled={!hasKey}
              onClick={() => toggleActiveEngine(p.id)}
              style={{
                padding: "8px 16px",
                borderRadius: "8px",
                fontSize: "13px",
                fontWeight: 600,
                cursor: hasKey ? "pointer" : "not-allowed",
                opacity: hasKey ? 1 : 0.45,
                border: activeProvider === p.id ? "1px solid #38bdf8" : "1px solid rgba(255, 255, 255, 0.1)",
                background: activeProvider === p.id ? "rgba(56, 189, 248, 0.15)" : "rgba(15, 23, 42, 0.6)",
                color: activeProvider === p.id ? "#38bdf8" : "#94a3b8",
                transition: "all 0.15s",
              }}
            >
              {hasKey ? "✓" : "○"} {p.name} {hasKey ? `(${srv?.models?.length || p.models.length} Models)` : ""}
            </button>
          );
        })}

        {customProviders.map((cp) => (
          <button
            key={cp.provider_id}
            type="button"
            onClick={() => toggleActiveEngine(cp.provider_id)}
            style={{
              padding: "8px 16px",
              borderRadius: "8px",
              fontSize: "13px",
              fontWeight: 600,
              cursor: "pointer",
              border: activeProvider === cp.provider_id ? "1px solid #a855f7" : "1px solid rgba(168, 85, 247, 0.3)",
              background: activeProvider === cp.provider_id ? "rgba(168, 85, 247, 0.2)" : "rgba(15, 23, 42, 0.6)",
              color: activeProvider === cp.provider_id ? "#c084fc" : "#a855f7",
            }}
          >
            ⚡ {cp.name} ({cp.models.length} Models)
          </button>
        ))}

        <button
          type="button"
          onClick={() => setShowCustomForm(!showCustomForm)}
          style={{
            padding: "8px 16px",
            borderRadius: "8px",
            fontSize: "13px",
            fontWeight: 700,
            cursor: "pointer",
            background: showCustomForm ? "rgba(236, 72, 153, 0.2)" : "linear-gradient(135deg, #a855f7 0%, #ec4899 100%)",
            border: showCustomForm ? "1px solid #ec4899" : "none",
            color: "#ffffff",
            marginLeft: "auto",
            display: "inline-flex",
            alignItems: "center",
            gap: "6px",
          }}
        >
          {showCustomForm ? "✕ Close Custom Form" : "+ Add Custom API (OmniRoute)"}
        </button>
      </div>

      {/* OmniRoute-Style Custom Endpoint Form */}
      {showCustomForm && (
        <div
          style={{
            background: "rgba(15, 23, 42, 0.95)",
            border: "1px solid rgba(168, 85, 247, 0.4)",
            borderRadius: "12px",
            padding: "24px",
            boxShadow: "0 10px 30px rgba(168, 85, 247, 0.15)",
            display: "flex",
            flexDirection: "column",
            gap: "18px",
          }}
        >
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
            <div>
              <div style={{ display: "inline-flex", alignItems: "center", gap: "6px", color: "#c084fc", fontSize: "12px", fontWeight: 700, textTransform: "uppercase" }}>
                <span>⚡</span> OmniRoute Custom Endpoint
              </div>
              <h3 style={{ margin: "4px 0 0 0", fontSize: "1.25rem", color: "#f8fafc", fontWeight: 700 }}>
                Connect Any OpenAI / Anthropic Compatible Endpoint
              </h3>
            </div>
            <span style={{ fontSize: "12px", color: "#94a3b8" }}>
              Supports vLLM, Ollama, Together AI, LiteLLM, Groq, LM Studio, etc.
            </span>
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))", gap: "16px" }}>
            <div>
              <label style={{ display: "block", fontSize: "12px", fontWeight: 600, color: "#cbd5e1", marginBottom: "6px" }}>
                Provider Name / Label *
              </label>
              <input
                type="text"
                placeholder="e.g. Together AI / Local vLLM / Ollama"
                value={customName}
                onChange={(e) => setCustomName(e.target.value)}
                style={{
                  width: "100%",
                  padding: "10px 12px",
                  background: "rgba(3, 7, 18, 0.8)",
                  border: "1px solid rgba(255, 255, 255, 0.12)",
                  borderRadius: "6px",
                  color: "#f8fafc",
                  fontSize: "13px",
                  boxSizing: "border-box",
                }}
              />
            </div>

            <div>
              <label style={{ display: "block", fontSize: "12px", fontWeight: 600, color: "#cbd5e1", marginBottom: "6px" }}>
                Base URL (Endpoint) *
              </label>
              <input
                type="text"
                placeholder="https://api.together.xyz/v1 or http://localhost:11434/v1"
                value={customBaseUrl}
                onChange={(e) => setCustomBaseUrl(e.target.value)}
                style={{
                  width: "100%",
                  padding: "10px 12px",
                  background: "rgba(3, 7, 18, 0.8)",
                  border: "1px solid rgba(255, 255, 255, 0.12)",
                  borderRadius: "6px",
                  color: "#f8fafc",
                  fontSize: "13px",
                  fontFamily: "monospace",
                  boxSizing: "border-box",
                }}
              />
            </div>

            <div>
              <label style={{ display: "block", fontSize: "12px", fontWeight: 600, color: "#cbd5e1", marginBottom: "6px" }}>
                API Key (Optional for local Ollama)
              </label>
              <input
                type="password"
                placeholder="sk-... or leave empty for local"
                value={customApiKey}
                onChange={(e) => setCustomApiKey(e.target.value)}
                style={{
                  width: "100%",
                  padding: "10px 12px",
                  background: "rgba(3, 7, 18, 0.8)",
                  border: "1px solid rgba(255, 255, 255, 0.12)",
                  borderRadius: "6px",
                  color: "#f8fafc",
                  fontSize: "13px",
                  fontFamily: "monospace",
                  boxSizing: "border-box",
                }}
              />
            </div>

            <div>
              <label style={{ display: "block", fontSize: "12px", fontWeight: 600, color: "#cbd5e1", marginBottom: "6px" }}>
                Protocol Standard
              </label>
              <select
                value={customProtocol}
                onChange={(e) => setCustomProtocol(e.target.value as "openai" | "anthropic")}
                style={{
                  width: "100%",
                  padding: "10px 12px",
                  background: "rgba(3, 7, 18, 0.8)",
                  border: "1px solid rgba(255, 255, 255, 0.12)",
                  borderRadius: "6px",
                  color: "#f8fafc",
                  fontSize: "13px",
                  boxSizing: "border-box",
                }}
              >
                <option value="openai">OpenAI Compatible (/v1/chat/completions)</option>
                <option value="anthropic">Anthropic Compatible (/v1/messages)</option>
              </select>
            </div>
          </div>

          <div>
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "6px" }}>
              <label style={{ fontSize: "12px", fontWeight: 600, color: "#cbd5e1" }}>
                Models (Comma-separated list of model IDs) *
              </label>
              <button
                type="button"
                disabled={customTesting}
                onClick={testAndFetchCustomModels}
                style={{
                  padding: "4px 10px",
                  borderRadius: "4px",
                  fontSize: "11px",
                  fontWeight: 600,
                  background: "rgba(56, 189, 248, 0.15)",
                  border: "1px solid rgba(56, 189, 248, 0.3)",
                  color: "#38bdf8",
                  cursor: customTesting ? "wait" : "pointer",
                }}
              >
                {customTesting ? "Testing..." : "⚡ Auto-Detect Models from URL"}
              </button>
            </div>
            <textarea
              rows={2}
              placeholder="e.g. meta-llama/Llama-3-70b-chat-hf, mistralai/Mixtral-8x7B-Instruct-v0.1, qwen2.5-coder:32b"
              value={customModels}
              onChange={(e) => setCustomModels(e.target.value)}
              style={{
                width: "100%",
                padding: "10px 12px",
                background: "rgba(3, 7, 18, 0.8)",
                border: "1px solid rgba(255, 255, 255, 0.12)",
                borderRadius: "6px",
                color: "#f8fafc",
                fontSize: "13px",
                fontFamily: "monospace",
                boxSizing: "border-box",
              }}
            />
            {customTestResult && (
              <div
                style={{
                  marginTop: "6px",
                  fontSize: "12px",
                  color: customTestResult.ok ? "#10b981" : "#f87171",
                  fontWeight: 500,
                }}
              >
                {customTestResult.ok ? "✓ " : "✕ "}
                {customTestResult.message}
              </div>
            )}
          </div>

          <div style={{ display: "flex", justifyContent: "flex-end", gap: "10px" }}>
            <button
              type="button"
              onClick={() => setShowCustomForm(false)}
              style={{
                padding: "8px 16px",
                borderRadius: "6px",
                fontSize: "13px",
                background: "rgba(255, 255, 255, 0.05)",
                border: "1px solid rgba(255, 255, 255, 0.1)",
                color: "#94a3b8",
                cursor: "pointer",
              }}
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={saveCustomProvider}
              style={{
                padding: "8px 20px",
                borderRadius: "6px",
                fontSize: "13px",
                fontWeight: 700,
                background: "linear-gradient(135deg, #a855f7 0%, #ec4899 100%)",
                border: "none",
                color: "#ffffff",
                cursor: "pointer",
              }}
            >
              Save Custom API to /v1/models
            </button>
          </div>
        </div>
      )}

      {/* Active Custom APIs List */}
      {customProviders.length > 0 && (
        <div style={{ display: "flex", flexDirection: "column", gap: "12px" }}>
          <h3 style={{ fontSize: "14px", fontWeight: 700, color: "#cbd5e1", margin: 0, textTransform: "uppercase", letterSpacing: "0.05em" }}>
            Configured Custom Endpoints ({customProviders.length})
          </h3>
          <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(320px, 1fr))", gap: "14px" }}>
            {customProviders.map((cp) => (
              <div
                key={cp.provider_id}
                style={{
                  background: "rgba(15, 23, 42, 0.75)",
                  border: "1px solid rgba(168, 85, 247, 0.3)",
                  borderRadius: "8px",
                  padding: "16px",
                  display: "flex",
                  flexDirection: "column",
                  gap: "10px",
                }}
              >
                <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
                  <div>
                    <span style={{ fontWeight: 700, color: "#f8fafc", fontSize: "15px" }}>{cp.name}</span>
                    <span style={{ marginLeft: "8px", fontSize: "11px", padding: "1px 6px", borderRadius: "4px", background: "rgba(168, 85, 247, 0.2)", color: "#c084fc" }}>
                      {cp.protocol.toUpperCase()}
                    </span>
                  </div>
                  <button
                    type="button"
                    onClick={() => clearKey(cp.provider_id, cp.name)}
                    style={{
                      background: "rgba(239, 68, 68, 0.15)",
                      border: "1px solid rgba(239, 68, 68, 0.3)",
                      color: "#ef4444",
                      borderRadius: "4px",
                      padding: "3px 8px",
                      fontSize: "11px",
                      cursor: "pointer",
                    }}
                  >
                    Delete
                  </button>
                </div>
                <div style={{ fontSize: "12px", fontFamily: "monospace", color: "#94a3b8", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                  URL: {cp.base_url}
                </div>
                <div style={{ display: "flex", flexWrap: "wrap", gap: "6px" }}>
                  {cp.models.map((m) => (
                    <span
                      key={m}
                      style={{
                        fontSize: "11px",
                        padding: "2px 8px",
                        borderRadius: "4px",
                        background: "rgba(255, 255, 255, 0.06)",
                        color: "#e2e8f0",
                        fontFamily: "monospace",
                      }}
                    >
                      {m}
                    </span>
                  ))}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Standard Providers Grid */}
      <div>
        <h3 style={{ fontSize: "14px", fontWeight: 700, color: "#cbd5e1", margin: "0 0 14px 0", textTransform: "uppercase", letterSpacing: "0.05em" }}>
          Frontier Model Providers (Direct Integration)
        </h3>
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "repeat(auto-fill, minmax(320px, 1fr))",
            gap: "18px",
          }}
        >
          {BYOK_PROVIDERS.map((provider) => {
            const srv = serverProviders.find((sp) => sp.provider_id === provider.id);
            const isSaved = !!srv?.has_key;
            const isVisible = !!visibleKeys[provider.id];
            const currentInput = inputValues[provider.id] || "";

            return (
              <div
                key={provider.id}
                style={{
                  background: "rgba(15, 23, 42, 0.65)",
                  border: isSaved ? "1px solid rgba(16, 185, 129, 0.4)" : "1px solid rgba(255, 255, 255, 0.08)",
                  borderRadius: "10px",
                  padding: "18px",
                  display: "flex",
                  flexDirection: "column",
                  justifyContent: "space-between",
                  gap: "14px",
                  boxShadow: isSaved ? "0 4px 20px rgba(16, 185, 129, 0.08)" : "none",
                }}
              >
                <div>
                  <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "8px" }}>
                    <span style={{ fontWeight: 700, fontSize: "16px", color: "#f8fafc" }}>{provider.name}</span>
                    <span
                      style={{
                        fontSize: "11px",
                        fontWeight: 600,
                        padding: "2px 8px",
                        borderRadius: "9999px",
                        background: isSaved ? "rgba(16, 185, 129, 0.15)" : "rgba(148, 163, 184, 0.1)",
                        color: isSaved ? "#10b981" : "#64748b",
                        border: isSaved ? "1px solid rgba(16, 185, 129, 0.3)" : "1px solid rgba(255, 255, 255, 0.06)",
                      }}
                    >
                      {isSaved ? `Active in /v1/models (${srv?.models?.length || provider.models.length} models)` : "Not Configured"}
                    </span>
                  </div>

                  <p style={{ fontSize: "12px", color: "#94a3b8", margin: "0 0 12px 0", lineHeight: 1.4 }}>
                    Models: {provider.models.slice(0, 4).join(", ")}
                    {provider.models.length > 4 ? ` +${provider.models.length - 4} more` : ""}
                  </p>

                  <div style={{ position: "relative" }}>
                    <input
                      type={isVisible ? "text" : "password"}
                      placeholder={isSaved && srv?.key_masked ? srv.key_masked : provider.placeholder}
                      value={currentInput}
                      onChange={(e) => setInputValues({ ...inputValues, [provider.id]: e.target.value })}
                      style={{
                        width: "100%",
                        padding: "10px 40px 10px 12px",
                        background: "rgba(3, 7, 18, 0.7)",
                        border: "1px solid rgba(255, 255, 255, 0.12)",
                        borderRadius: "6px",
                        color: "#f8fafc",
                        fontSize: "13px",
                        fontFamily: "monospace",
                        boxSizing: "border-box",
                      }}
                    />
                    <button
                      type="button"
                      onClick={() => setVisibleKeys({ ...visibleKeys, [provider.id]: !isVisible })}
                      style={{
                        position: "absolute",
                        right: "8px",
                        top: "50%",
                        transform: "translateY(-50%)",
                        background: "transparent",
                        border: "none",
                        color: "#94a3b8",
                        fontSize: "12px",
                        cursor: "pointer",
                        padding: "4px",
                      }}
                    >
                      {isVisible ? "Hide" : "Show"}
                    </button>
                  </div>
                </div>

                <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", paddingTop: "8px", borderTop: "1px solid rgba(255, 255, 255, 0.06)" }}>
                  <a
                    href={provider.docsUrl}
                    target="_blank"
                    rel="noopener noreferrer"
                    style={{ fontSize: "12px", color: "#38bdf8", textDecoration: "none" }}
                  >
                    Get API Key ↗
                  </a>

                  <div style={{ display: "flex", gap: "8px" }}>
                    {isSaved && (
                      <button
                        type="button"
                        onClick={() => clearKey(provider.id, provider.name)}
                        style={{
                          padding: "6px 12px",
                          borderRadius: "6px",
                          fontSize: "12px",
                          background: "rgba(239, 68, 68, 0.15)",
                          border: "1px solid rgba(239, 68, 68, 0.3)",
                          color: "#ef4444",
                          cursor: "pointer",
                        }}
                      >
                        Remove
                      </button>
                    )}
                    <button
                      type="button"
                      onClick={() => saveStandardKey(provider)}
                      style={{
                        padding: "6px 14px",
                        borderRadius: "6px",
                        fontSize: "12px",
                        fontWeight: 600,
                        background: "linear-gradient(135deg, #0284c7 0%, #6366f1 100%)",
                        border: "none",
                        color: "#ffffff",
                        cursor: "pointer",
                      }}
                    >
                      {isSaved ? "Update Key" : "Save & Activate"}
                    </button>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}
