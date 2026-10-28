"use client";

import React, { useState, useEffect } from "react";
import { VarynthLogo } from "./VarynthLogo";

export function ResponsiveNav({ signedIn }: { signedIn: boolean }) {
  const [menuOpen, setMenuOpen] = useState(false);

  // Close menu on route change or Escape key
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") setMenuOpen(false);
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

  // Lock body scroll when mobile menu is open
  useEffect(() => {
    if (menuOpen) {
      document.body.style.overflow = "hidden";
    } else {
      document.body.style.overflow = "";
    }
    return () => {
      document.body.style.overflow = "";
    };
  }, [menuOpen]);

  return (
    <div className="nav-wrap" style={{ position: "sticky", top: 0, zIndex: 50, backdropFilter: "blur(18px)", WebkitBackdropFilter: "blur(18px)", background: "rgba(3, 7, 18, 0.75)", borderBottom: "1px solid rgba(255, 255, 255, 0.08)" }}>
      <div className="shell">
        <nav className="nav" aria-label="Varynth primary navigation" style={{ display: "flex", alignItems: "center", justifyContent: "space-between", height: "64px", padding: "0 16px" }}>
          <a className="brand-group" href="/" style={{ textDecoration: "none", display: "inline-flex", alignItems: "center" }}>
            <VarynthLogo size={32} />
          </a>

          {/* Desktop Nav Links */}
          <div className="nav-links desktop-nav-links" style={{ display: "flex", alignItems: "center", gap: "24px" }}>
            <a href="/docs" style={{ color: "#cbd5e1", fontSize: "14px", fontWeight: 500, textDecoration: "none", transition: "color 0.2s" }}>
              Documentation
            </a>
            <a href="/models" style={{ color: "#cbd5e1", fontSize: "14px", fontWeight: 500, textDecoration: "none", transition: "color 0.2s" }}>
              Models &amp; Skills
            </a>
            <a href="/pricing" style={{ color: "#cbd5e1", fontSize: "14px", fontWeight: 500, textDecoration: "none", transition: "color 0.2s" }}>
              Pricing
            </a>
            {signedIn ? (
              <a
                className="nav-cta"
                href="/dashboard"
                style={{
                  background: "linear-gradient(135deg, #0284c7 0%, #6366f1 100%)",
                  color: "#ffffff",
                  fontSize: "13px",
                  fontWeight: 600,
                  padding: "8px 18px",
                  borderRadius: "8px",
                  textDecoration: "none",
                  boxShadow: "0 2px 14px rgba(2, 132, 199, 0.35)",
                  transition: "transform 0.15s, box-shadow 0.15s"
                }}
              >
                Dashboard
              </a>
            ) : (
              <div style={{ display: "flex", alignItems: "center", gap: "14px" }}>
                <a href="/login" style={{ color: "#cbd5e1", fontSize: "14px", fontWeight: 500, textDecoration: "none" }}>
                  Sign in
                </a>
                <a
                  className="nav-cta"
                  href="/register"
                  style={{
                    background: "linear-gradient(135deg, #0284c7 0%, #6366f1 100%)",
                    color: "#ffffff",
                    fontSize: "13px",
                    fontWeight: 600,
                    padding: "8px 18px",
                    borderRadius: "8px",
                    textDecoration: "none",
                    boxShadow: "0 2px 14px rgba(2, 132, 199, 0.35)"
                  }}
                >
                  Create account
                </a>
              </div>
            )}
          </div>

          {/* Mobile Hamburger Toggle */}
          <button
            type="button"
            className="mobile-hamburger"
            aria-label={menuOpen ? "Close menu" : "Open menu"}
            aria-expanded={menuOpen}
            onClick={() => setMenuOpen(!menuOpen)}
            style={{
              display: "none",
              background: "transparent",
              border: "none",
              cursor: "pointer",
              padding: "8px",
              color: "#f8fafc",
              zIndex: 60
            }}
          >
            <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              {menuOpen ? (
                <>
                  <line x1="18" y1="6" x2="6" y2="18" />
                  <line x1="6" y1="6" x2="18" y2="18" />
                </>
              ) : (
                <>
                  <line x1="4" y1="6" x2="20" y2="6" />
                  <line x1="4" y1="12" x2="20" y2="12" />
                  <line x1="4" y1="18" x2="20" y2="18" />
                </>
              )}
            </svg>
          </button>
        </nav>
      </div>

      {/* Mobile Drawer Overlay */}
      {menuOpen && (
        <div
          className="mobile-drawer-overlay"
          style={{
            position: "fixed",
            top: "64px",
            left: 0,
            right: 0,
            bottom: 0,
            height: "calc(100vh - 64px)",
            backgroundColor: "rgba(3, 7, 18, 0.94)",
            backdropFilter: "blur(24px)",
            WebkitBackdropFilter: "blur(24px)",
            zIndex: 55,
            padding: "24px 20px",
            display: "flex",
            flexDirection: "column",
            justifyContent: "space-between",
            overflowY: "auto"
          }}
        >
          <div style={{ display: "flex", flexDirection: "column", gap: "18px" }}>
            <a
              href="/docs"
              onClick={() => setMenuOpen(false)}
              style={{ fontSize: "18px", fontWeight: 600, color: "#f8fafc", textDecoration: "none", padding: "10px 0", borderBottom: "1px solid rgba(255,255,255,0.06)" }}
            >
              Documentation
            </a>
            <a
              href="/models"
              onClick={() => setMenuOpen(false)}
              style={{ fontSize: "18px", fontWeight: 600, color: "#f8fafc", textDecoration: "none", padding: "10px 0", borderBottom: "1px solid rgba(255,255,255,0.06)" }}
            >
              Models &amp; Skills
            </a>
            <a
              href="/pricing"
              onClick={() => setMenuOpen(false)}
              style={{ fontSize: "18px", fontWeight: 600, color: "#f8fafc", textDecoration: "none", padding: "10px 0", borderBottom: "1px solid rgba(255,255,255,0.06)" }}
            >
              Pricing
            </a>
            {signedIn ? (
              <a
                href="/dashboard"
                onClick={() => setMenuOpen(false)}
                style={{
                  fontSize: "16px",
                  fontWeight: 600,
                  color: "#38bdf8",
                  textDecoration: "none",
                  padding: "12px 16px",
                  borderRadius: "8px",
                  background: "rgba(56, 189, 248, 0.1)",
                  border: "1px solid rgba(56, 189, 248, 0.2)",
                  marginTop: "8px"
                }}
              >
                Go to Dashboard →
              </a>
            ) : (
              <div style={{ display: "flex", flexDirection: "column", gap: "12px", marginTop: "12px" }}>
                <a
                  href="/login"
                  onClick={() => setMenuOpen(false)}
                  style={{
                    fontSize: "16px",
                    fontWeight: 600,
                    color: "#f8fafc",
                    textDecoration: "none",
                    textAlign: "center",
                    padding: "12px 16px",
                    borderRadius: "8px",
                    background: "rgba(255, 255, 255, 0.06)",
                    border: "1px solid rgba(255, 255, 255, 0.12)"
                  }}
                >
                  Sign in
                </a>
                <a
                  href="/register"
                  onClick={() => setMenuOpen(false)}
                  style={{
                    fontSize: "16px",
                    fontWeight: 600,
                    color: "#ffffff",
                    textDecoration: "none",
                    textAlign: "center",
                    padding: "12px 16px",
                    borderRadius: "8px",
                    background: "linear-gradient(135deg, #0284c7 0%, #6366f1 100%)",
                    boxShadow: "0 2px 16px rgba(2, 132, 199, 0.35)"
                  }}
                >
                  Create account
                </a>
              </div>
            )}
          </div>

          <div style={{ paddingTop: "24px", borderTop: "1px solid rgba(255, 255, 255, 0.08)", display: "flex", justifyContent: "space-between", fontSize: "12px", color: "#64748b" }}>
            <span>Varynth Singularity v0.1.3</span>
            <span>61 Models Live</span>
          </div>
        </div>
      )}
    </div>
  );
}
