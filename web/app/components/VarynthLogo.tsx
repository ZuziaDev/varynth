"use client";

import React from "react";

export function VarynthLogo({ size = 32 }: { size?: number }) {
  return (
    <span className="brand-lockup" style={{ display: "inline-flex", alignItems: "center", gap: "10px", textDecoration: "none" }}>
      <svg
        width={size}
        height={size}
        viewBox="0 0 48 48"
        fill="none"
        xmlns="http://www.w3.org/2000/svg"
        className="brand-mark-svg"
        aria-hidden="true"
        style={{ flexShrink: 0 }}
      >
        <defs>
          <linearGradient id="vCyanVioletReact" x1="4" y1="4" x2="44" y2="44" gradientUnits="userSpaceOnUse">
            <stop offset="0%" stopColor="#38bdf8" />
            <stop offset="50%" stopColor="#818cf8" />
            <stop offset="100%" stopColor="#c084fc" />
          </linearGradient>
          <linearGradient id="vCoreGlowReact" x1="12" y1="8" x2="36" y2="40" gradientUnits="userSpaceOnUse">
            <stop offset="0%" stopColor="#38bdf8" stopOpacity="0.35" />
            <stop offset="100%" stopColor="#818cf8" stopOpacity="0.05" />
          </linearGradient>
          <linearGradient id="vFacetLeftReact" x1="8" y1="10" x2="24" y2="42" gradientUnits="userSpaceOnUse">
            <stop offset="0%" stopColor="#38bdf8" stopOpacity="0.85" />
            <stop offset="100%" stopColor="#6366f1" stopOpacity="0.35" />
          </linearGradient>
          <linearGradient id="vFacetRightReact" x1="40" y1="10" x2="24" y2="42" gradientUnits="userSpaceOnUse">
            <stop offset="0%" stopColor="#c084fc" stopOpacity="0.85" />
            <stop offset="100%" stopColor="#818cf8" stopOpacity="0.35" />
          </linearGradient>
          <filter id="vGlowReact" x="-20%" y="-20%" width="140%" height="140%">
            <feGaussianBlur stdDeviation="2.5" result="blur" />
            <feComposite in="SourceGraphic" in2="blur" operator="over" />
          </filter>
        </defs>

        {/* Orbital Quantum Ring */}
        <circle cx="24" cy="24" r="21" stroke="url(#vCyanVioletReact)" strokeWidth="1.2" strokeDasharray="6 3.5" opacity="0.65" />
        <circle cx="24" cy="24" r="18" stroke="url(#vCyanVioletReact)" strokeWidth="0.8" opacity="0.25" />

        {/* Outer Singularity Shield Hexagon */}
        <polygon
          points="24,5 40.5,14.5 40.5,33.5 24,43 7.5,33.5 7.5,14.5"
          fill="url(#vCoreGlowReact)"
          stroke="url(#vCyanVioletReact)"
          strokeWidth="1.4"
          strokeLinejoin="round"
        />

        {/* Holographic Facet Wings */}
        <path d="M7.5 14.5L24 24V43L7.5 33.5V14.5Z" fill="url(#vFacetLeftReact)" opacity="0.45" />
        <path d="M40.5 14.5L24 24V43L40.5 33.5V14.5Z" fill="url(#vFacetRightReact)" opacity="0.3" />

        {/* Precision Converging Quantum 'V' */}
        <path
          d="M13 14L24 36L35 14"
          stroke="#ffffff"
          strokeWidth="2.8"
          strokeLinecap="round"
          strokeLinejoin="round"
          filter="url(#vGlowReact)"
        />
        <path
          d="M17 14L24 30L31 14"
          stroke="url(#vCyanVioletReact)"
          strokeWidth="1.8"
          strokeLinecap="round"
          strokeLinejoin="round"
        />

        {/* Apex Core Nodes */}
        <circle cx="24" cy="5" r="2" fill="#38bdf8" />
        <circle cx="24" cy="24" r="2.5" fill="#ffffff" />
        <circle cx="24" cy="36" r="2.2" fill="#c084fc" />
        <circle cx="13" cy="14" r="2" fill="#38bdf8" />
        <circle cx="35" cy="14" r="2" fill="#c084fc" />
      </svg>
      <span className="brand-text-group" style={{ display: "inline-flex", alignItems: "baseline", gap: "6px" }}>
        <span
          className="brand-title"
          style={{
            fontFamily: "var(--font-space), -apple-system, sans-serif",
            fontWeight: 800,
            fontSize: "1.25rem",
            letterSpacing: "-0.03em",
            background: "linear-gradient(135deg, #ffffff 0%, #38bdf8 60%, #c084fc 100%)",
            WebkitBackgroundClip: "text",
            WebkitTextFillColor: "transparent",
            textShadow: "0 0 20px rgba(56, 189, 248, 0.25)"
          }}
        >
          Varynth
        </span>
        <span
          className="brand-badge"
          style={{
            fontSize: "0.68rem",
            fontWeight: 600,
            letterSpacing: "0.06em",
            textTransform: "uppercase",
            padding: "2px 6px",
            borderRadius: "9999px",
            background: "rgba(56, 189, 248, 0.12)",
            border: "1px solid rgba(56, 189, 248, 0.25)",
            color: "#38bdf8"
          }}
        >
          v0.1.3
        </span>
      </span>
    </span>
  );
}
