"use client";

import React, { useEffect, useRef, useState } from "react";
import { VarynthLogo } from "./VarynthLogo";

const LINKS = [
  { href: "/docs", label: "Documentation" },
  { href: "/models", label: "Models & Skills" },
  { href: "/pricing", label: "Pricing" },
];

export function ResponsiveNav({ signedIn }: { signedIn: boolean }) {
  const [menuOpen, setMenuOpen] = useState(false);
  const toggleRef = useRef<HTMLButtonElement>(null);

  // Escape closes the drawer and returns focus to the toggle.
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        setMenuOpen((open) => {
          if (open) toggleRef.current?.focus();
          return false;
        });
      }
    }
    // Leaving the mobile breakpoint closes the drawer.
    const mq = window.matchMedia("(min-width: 821px)");
    function onMq(e: MediaQueryListEvent) {
      if (e.matches) setMenuOpen(false);
    }
    window.addEventListener("keydown", onKey);
    // Safari < 14 exposes only the legacy addListener/removeListener API.
    let unsubscribeMq: () => void = () => {};
    if (typeof mq.addEventListener === "function") {
      mq.addEventListener("change", onMq);
      unsubscribeMq = () => mq.removeEventListener("change", onMq);
    } else if (typeof mq.addListener === "function") {
      mq.addListener(onMq);
      unsubscribeMq = () => mq.removeListener(onMq);
    }
    return () => {
      window.removeEventListener("keydown", onKey);
      unsubscribeMq();
    };
  }, []);

  // Lock body scroll while the drawer is open.
  useEffect(() => {
    document.body.style.overflow = menuOpen ? "hidden" : "";
    return () => {
      document.body.style.overflow = "";
    };
  }, [menuOpen]);

  const close = () => setMenuOpen(false);

  return (
    <div className="nav-wrap">
      <div className="shell">
        <nav className="nav nav-bar" aria-label="Varynth primary navigation">
          <a className="brand-group" href="/" aria-label="Varynth home">
            <VarynthLogo size={32} />
          </a>

          <div className="nav-links desktop-nav-links">
            {LINKS.map((l) => (
              <a key={l.href} href={l.href}>
                {l.label}
              </a>
            ))}
            {signedIn ? (
              <a className="nav-cta" href="/dashboard">
                Dashboard
              </a>
            ) : (
              <>
                <a href="/login">Sign in</a>
                <a className="nav-cta" href="/register">
                  Create account
                </a>
              </>
            )}
          </div>

          <button
            ref={toggleRef}
            type="button"
            className="mobile-hamburger"
            aria-label={menuOpen ? "Close menu" : "Open menu"}
            aria-expanded={menuOpen}
            aria-controls="mobile-drawer"
            onClick={() => setMenuOpen((o) => !o)}
          >
            <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
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

      {menuOpen && (
        <div id="mobile-drawer" className="mobile-drawer-overlay">
          <div className="mobile-drawer-links">
            {LINKS.map((l) => (
              <a key={l.href} href={l.href} onClick={close}>
                {l.label}
              </a>
            ))}
          </div>
          <div className="mobile-drawer-actions">
            {signedIn ? (
              <a className="btn" href="/dashboard" onClick={close}>
                Go to dashboard
              </a>
            ) : (
              <>
                <a className="btn-quiet" href="/login" onClick={close}>
                  Sign in
                </a>
                <a className="btn" href="/register" onClick={close}>
                  Create account
                </a>
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
