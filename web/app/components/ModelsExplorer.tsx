"use client";

import React, { useMemo, useState } from "react";
import type { PublicModel } from "../../lib/api";

type TierFilter = "all" | "free" | "pro";
type CategoryFilter = "all" | "reasoning" | "coding" | "vision" | "fast" | "image";

function getModelCategory(m: PublicModel): CategoryFilter {
  if (m.category) {
    const cat = m.category.toLowerCase() as CategoryFilter;
    if (["reasoning", "coding", "vision", "fast", "image"].includes(cat)) {
      return cat;
    }
  }
  const id = m.id.toLowerCase();
  const name = m.display_name.toLowerCase();
  if (id.includes("flux") || id.includes("image") || id.includes("imagen") || id.includes("ideogram")) {
    return "image";
  }
  if (id.includes("coder") || id.includes("llama-3.3-70b") || id.includes("granite") || name.includes("coding")) {
    return "coding";
  }
  if (
    name.includes("vision") ||
    id.includes("scout") ||
    id.includes("mistral-small") ||
    id.includes("gemma-4")
  ) {
    return "vision";
  }
  if (
    name.includes("reasoning") ||
    id.includes("gpt-oss-120b") ||
    id.includes("nemotron") ||
    id.includes("deepseek-r1") ||
    id.includes("qwq") ||
    id.includes("qwen3") ||
    id.includes("claude-opus") ||
    id.includes("gpt-5")
  ) {
    return "reasoning";
  }
  return "fast";
}

function isModelFree(m: PublicModel): boolean {
  if (m.min_plan) return m.min_plan === "free";
  if (m.pricing) return m.pricing.is_free;
  return m.tier === "public";
}

export function ModelsExplorer({ models }: { models: PublicModel[] }) {
  const [tier, setTier] = useState<TierFilter>("all");
  const [category, setCategory] = useState<CategoryFilter>("all");
  const [query, setQuery] = useState("");
  const [copiedId, setCopiedId] = useState<string | null>(null);

  const counts = useMemo(() => {
    const byTier = { all: models.length, free: 0, pro: 0 };
    const byCat = { all: 0, reasoning: 0, coding: 0, vision: 0, fast: 0, image: 0 };

    for (const m of models) {
      const free = isModelFree(m);
      if (free) byTier.free++;
      else byTier.pro++;

      // When tier is filtered, count categories within that tier
      if (tier === "all" || (tier === "free" && free) || (tier === "pro" && !free)) {
        byCat.all++;
        const c = getModelCategory(m);
        byCat[c] = (byCat[c] || 0) + 1;
      }
    }
    return { byTier, byCat };
  }, [models, tier]);

  const filtered = useMemo(() => {
    return models.filter((m) => {
      const free = isModelFree(m);
      if (tier === "free" && !free) return false;
      if (tier === "pro" && free) return false;
      if (category !== "all" && getModelCategory(m) !== category) return false;
      if (query.trim()) {
        const q = query.trim().toLowerCase();
        return (
          m.id.toLowerCase().includes(q) ||
          m.display_name.toLowerCase().includes(q) ||
          (m.category && m.category.toLowerCase().includes(q))
        );
      }
      return true;
    });
  }, [models, tier, category, query]);

  const copyModel = async (id: string) => {
    try {
      await navigator.clipboard.writeText(id);
      setCopiedId(id);
      setTimeout(() => setCopiedId(null), 1500);
    } catch {
      // ignore
    }
  };

  const tierTabs: { id: TierFilter; label: string; count: number }[] = [
    { id: "all", label: "All Models", count: counts.byTier.all },
    { id: "free", label: "Free Models", count: counts.byTier.free },
    { id: "pro", label: "Pro / Paid Models", count: counts.byTier.pro },
  ];

  const catTabs: { id: CategoryFilter; label: string; count: number }[] = [
    { id: "all", label: "All Types", count: counts.byCat.all },
    { id: "reasoning", label: "Reasoning", count: counts.byCat.reasoning },
    { id: "coding", label: "Coding & Tools", count: counts.byCat.coding },
    { id: "vision", label: "Vision", count: counts.byCat.vision },
    { id: "fast", label: "Fast & Compact", count: counts.byCat.fast },
    { id: "image", label: "Image Synthesis", count: counts.byCat.image },
  ];

  return (
    <div className="models-explorer">
      <div className="models-toolbar" style={{ display: "flex", flexDirection: "column", gap: "var(--space-md)" }}>
        {/* Tier filter tabs */}
        <div style={{ display: "flex", flexWrap: "wrap", gap: "var(--space-xs)", alignItems: "center" }}>
          <span style={{ fontSize: "var(--text-xs)", color: "var(--color-muted)", fontWeight: 600, marginRight: "4px" }}>
            PLAN:
          </span>
          {tierTabs.map((t) => (
            <button
              key={t.id}
              type="button"
              className={`filter-pill ${tier === t.id ? "active" : ""}`}
              onClick={() => {
                setTier(t.id);
                setCategory("all");
              }}
            >
              <span>{t.label}</span>
              <span className="filter-pill-count">{t.count}</span>
            </button>
          ))}
        </div>

        {/* Capability filter pills & search */}
        <div style={{ display: "flex", justifyContent: "space-between", flexWrap: "wrap", gap: "var(--space-sm)", alignItems: "center" }}>
          <div className="models-filter-pills" role="tablist" aria-label="Filter models by capability">
            {catTabs.map((t) => (
              <button
                key={t.id}
                type="button"
                role="tab"
                aria-selected={category === t.id}
                className={`filter-pill ${category === t.id ? "active" : ""}`}
                onClick={() => setCategory(t.id)}
              >
                <span>{t.label}</span>
                <span className="filter-pill-count">{t.count}</span>
              </button>
            ))}
          </div>

          <div className="models-search-wrap" style={{ minWidth: "280px", flex: 1, maxWidth: "420px" }}>
            <input
              type="search"
              className="models-search-input"
              placeholder={`Filter ${filtered.length} of ${models.length} models...`}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              aria-label="Search models"
            />
          </div>
        </div>
      </div>

      <ul className="rows" style={{ marginTop: "var(--space-md)" }}>
        {filtered.map((model) => {
          const free = isModelFree(model);
          const cat = getModelCategory(model);
          const isCopied = copiedId === model.id;
          const cost = model.limits?.request_cost ?? model.pricing?.request_cost ?? 1;
          const rpm = model.limits?.rpm ?? (free ? 20 : 30);
          const rpd = model.limits?.rpd ?? (free ? 100 : 500);

          return (
            <li
              className="row row-interactive"
              key={model.id}
              onClick={() => copyModel(model.id)}
              title="Click to copy model identifier"
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "2px" }}>
                <span className="row-id">{model.id}</span>
                <span style={{ fontSize: "11px", color: "var(--color-muted)" }}>
                  {cost}x req · {rpm} RPM · {rpd} RPD
                </span>
              </div>

              <div style={{ display: "flex", flexDirection: "column", gap: "2px" }}>
                <span className="row-name">{model.display_name}</span>
                <span style={{ fontSize: "11px", color: free ? "var(--color-cool)" : "var(--color-accent)" }}>
                  {free
                    ? "Free quota ($0.00 / 1M tokens)"
                    : `$${model.pricing?.input_per_m?.toFixed(2) ?? "0.00"} in / $${model.pricing?.output_per_m?.toFixed(2) ?? "0.00"} out / 1M`}
                </span>
              </div>

              <div style={{ display: "flex", gap: "6px", alignItems: "center", justifySelf: "end" }}>
                {isCopied ? (
                  <span className="tag tag-free">copied</span>
                ) : (
                  <>
                    <span className="tag" style={{ textTransform: "capitalize" }}>
                      {cat}
                    </span>
                    <span className={`tag ${free ? "tag-free" : "tag-pro"}`}>
                      {free ? "FREE" : "PRO"}
                    </span>
                  </>
                )}
              </div>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
