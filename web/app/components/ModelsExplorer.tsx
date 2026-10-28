"use client";

import React, { useMemo, useState } from "react";
import type { PublicModel } from "../../lib/api";

type CategoryFilter = "all" | "reasoning" | "coding" | "vision" | "fast";

function classifyModel(m: PublicModel): CategoryFilter {
  const id = m.id.toLowerCase();
  const name = m.display_name.toLowerCase();
  if (id.includes("coder") || id.includes("llama-3.3-70b") || id.includes("granite")) {
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
    id.includes("qwen3")
  ) {
    return "reasoning";
  }
  return "fast";
}

export function ModelsExplorer({ models }: { models: PublicModel[] }) {
  const [category, setCategory] = useState<CategoryFilter>("all");
  const [query, setQuery] = useState("");
  const [copiedId, setCopiedId] = useState<string | null>(null);

  const counts = useMemo(() => {
    const c = { all: models.length, reasoning: 0, coding: 0, vision: 0, fast: 0 };
    for (const m of models) {
      c[classifyModel(m)]++;
    }
    return c;
  }, [models]);

  const filtered = useMemo(() => {
    return models.filter((m) => {
      if (category !== "all" && classifyModel(m) !== category) return false;
      if (query.trim()) {
        const q = query.trim().toLowerCase();
        return (
          m.id.toLowerCase().includes(q) ||
          m.display_name.toLowerCase().includes(q)
        );
      }
      return true;
    });
  }, [models, category, query]);

  const copyModel = async (id: string) => {
    try {
      await navigator.clipboard.writeText(id);
      setCopiedId(id);
      setTimeout(() => setCopiedId(null), 1500);
    } catch {
      // ignore
    }
  };

  const tabs: { id: CategoryFilter; label: string; count: number }[] = [
    { id: "all", label: "All verified models", count: counts.all },
    { id: "reasoning", label: "Flagship & Deep Reasoning", count: counts.reasoning },
    { id: "coding", label: "Coding & Tool Execution", count: counts.coding },
    { id: "vision", label: "Vision & Multimodal Chat", count: counts.vision },
    { id: "fast", label: "Low-Latency & Compact", count: counts.fast },
  ];

  return (
    <div className="models-explorer">
      <div className="models-toolbar">
        <div className="models-filter-pills" role="tablist" aria-label="Filter models by capability">
          {tabs.map((t) => (
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

        <div className="models-search-wrap">
          <input
            type="search"
            className="models-search-input"
            placeholder={`Filter ${models.length} verified models (e.g. gpt-oss, qwen, deepseek, llama, gemma)...`}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            aria-label="Search models"
          />
        </div>
      </div>

      <ul className="rows">
        {filtered.map((model) => {
          const cat = classifyModel(model);
          const isCopied = copiedId === model.id;
          return (
            <li
              className="row row-interactive"
              key={model.id}
              onClick={() => copyModel(model.id)}
              title="Click to copy model identifier"
            >
              <span className="row-id">{model.id}</span>
              <span className="row-name">{model.display_name}</span>
              <span style={{ display: "flex", gap: "8px", alignItems: "center" }}>
                <span className={`tag ${cat === "reasoning" || cat === "coding" ? "tag-pro" : "tag-free"}`}>
                  {isCopied ? "copied" : cat}
                </span>
              </span>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
