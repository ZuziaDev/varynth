"use client";

import { FormEvent, useState } from "react";

type Kind = "login" | "register";

export function AuthForm({
  kind,
  nextHref,
  initialError,
}: {
  kind: Kind;
  nextHref: string;
  initialError?: string;
}) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [state, setState] = useState<"idle" | "loading" | "error" | "success">(initialError ? "error" : "idle");
  const [message, setMessage] = useState(initialError || "");
  const [oauthNotice, setOauthNotice] = useState<{ provider: string; active: boolean } | null>(null);
  const minimumPassword = kind === "register" ? 12 : 1;

  function onOAuthProviderClick(provider: "GitHub" | "Google") {
    if (provider === "Google") {
      const clientId = process.env.NEXT_PUBLIC_GOOGLE_CLIENT_ID || "915835364102-99k83ic3dj4b1iq1lm1tl6ucj5jcf9i5.apps.googleusercontent.com";
      const redirectUri = `${window.location.origin}/api/auth/callback/google`;
      const url = `https://accounts.google.com/o/oauth2/v2/auth?client_id=${encodeURIComponent(clientId)}&redirect_uri=${encodeURIComponent(redirectUri)}&response_type=code&scope=openid%20email%20profile&prompt=select_account&state=${encodeURIComponent(nextHref)}`;
      window.location.href = url;
      return;
    }
    if (provider === "GitHub") {
      const clientId = process.env.NEXT_PUBLIC_GITHUB_CLIENT_ID || "Ov23limeLLdE2QbuxXCA";
      const redirectUri = `${window.location.origin}/api/auth/callback/github`;
      const url = `https://github.com/login/oauth/authorize?client_id=${encodeURIComponent(clientId)}&redirect_uri=${encodeURIComponent(redirectUri)}&scope=user:email&state=${encodeURIComponent(nextHref)}`;
      window.location.href = url;
      return;
    }
    setOauthNotice({ provider, active: true });
    setState("idle");
  }

  function proceedOAuth(provider: string) {
    const route = provider.toLowerCase();
    // Attempt redirect to OAuth provider endpoint or notify if credentials not configured
    window.location.href = `/api/auth/oauth/${route}?next=${encodeURIComponent(nextHref)}`;
  }

  function useDemoCredentials() {
    setEmail("dev@zuzia.dev");
    setPassword("VarynthSingularity2026!");
    setOauthNotice(null);
  }

  async function onSubmit(event: FormEvent) {
    event.preventDefault();
    if (email.trim().length === 0 || password.length < minimumPassword) {
      setState("error");
      setMessage(
        email.trim().length === 0
          ? "Email is required."
          : kind === "register"
            ? "Password must be at least 12 characters."
            : "Password is required.",
      );
      return;
    }
    setState("loading");
    setMessage("");
    try {
      const response = await fetch(kind === "login" ? "/api/auth/login" : "/api/auth/register", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ email, password }),
      });
      const body = (await response.json().catch(() => ({}))) as { error?: { message?: string } };
      if (!response.ok) {
        setState("error");
        setMessage(body.error?.message ?? "Could not sign in.");
        return;
      }
      if (kind === "register") {
        const login = await fetch("/api/auth/login", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ email, password }),
        });
        if (!login.ok) {
          setState("error");
          setMessage("Account created. Sign in failed.");
          return;
        }
      }
      setState("success");
      setMessage(kind === "login" ? "Signed in." : "Account created.");
      window.location.assign(nextHref);
    } catch {
      setState("error");
      setMessage("Network error. Try again.");
    }
  }

  const label =
    state === "loading"
      ? "Working..."
      : state === "success"
        ? "Signed in"
        : state === "error"
          ? "Try again"
          : kind === "login"
            ? "Sign in"
            : "Create account";

  return (
    <div className="auth-wrap">
      <form className="form-card" onSubmit={onSubmit} noValidate>
        <h1 style={{ fontSize: "1.75rem", fontWeight: 800, letterSpacing: "-0.02em" }}>
          {kind === "login" ? "Sign in to Varynth" : "Create your account"}
        </h1>
        <p className="form-subtitle">
          {kind === "login"
            ? "Access your sovereign models, BYOK key vault, Pro skills, and CLI sessions."
            : "Register with an email and a 12+ character password."}
        </p>

        {/* OAuth SSO Buttons */}
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
            gap: "10px",
            marginBottom: "16px",
          }}
        >
          <button
            type="button"
            className="btn-quiet"
            style={{
              width: "100%",
              display: "inline-flex",
              alignItems: "center",
              justifyContent: "center",
              gap: "8px",
              padding: "10px 14px",
              borderRadius: "8px",
              border: "1px solid rgba(255, 255, 255, 0.12)",
              background: "rgba(15, 23, 42, 0.6)",
              color: "#f8fafc",
              fontSize: "13px",
              fontWeight: 600,
              cursor: "pointer",
              transition: "all 0.15s ease",
            }}
            onClick={() => onOAuthProviderClick("GitHub")}
          >
            {/* GitHub SVG */}
            <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor">
              <path d="M12 0C5.37 0 0 5.37 0 12c0 5.31 3.435 9.795 8.205 11.385.6.105.825-.255.825-.57 0-.285-.015-1.23-.015-2.235-3.015.555-3.795-.735-4.035-1.41-.135-.345-.72-1.41-1.23-1.695-.42-.225-1.02-.78-.015-.795.945-.015 1.62.87 1.845 1.23 1.08 1.815 2.805 1.305 3.495.99.105-.78.42-1.305.765-1.605-2.67-.3-5.46-1.335-5.46-5.925 0-1.305.465-2.385 1.23-3.225-.12-.3-.54-1.53.12-3.18 0 0 1.005-.315 3.3 1.23.96-.27 1.98-.405 3-.405s2.04.135 3 .405c2.295-1.56 3.3-1.23 3.3-1.23.66 1.65.24 2.88.12 3.18.765.84 1.23 1.905 1.23 3.225 0 4.605-2.805 5.625-5.475 5.925.435.375.81 1.095.81 2.22 0 1.605-.015 2.895-.015 3.3 0 .315.225.69.825.57A12.02 12.02 0 0024 12c0-6.63-5.37-12-12-12z" />
            </svg>
            <span>GitHub</span>
          </button>

          <button
            type="button"
            className="btn-quiet"
            style={{
              width: "100%",
              display: "inline-flex",
              alignItems: "center",
              justifyContent: "center",
              gap: "8px",
              padding: "10px 14px",
              borderRadius: "8px",
              border: "1px solid rgba(255, 255, 255, 0.12)",
              background: "rgba(15, 23, 42, 0.6)",
              color: "#f8fafc",
              fontSize: "13px",
              fontWeight: 600,
              cursor: "pointer",
              transition: "all 0.15s ease",
            }}
            onClick={() => onOAuthProviderClick("Google")}
          >
            {/* Google SVG */}
            <svg width="18" height="18" viewBox="0 0 24 24">
              <path fill="#4285F4" d="M23.745 12.27c0-.7-.06-1.4-.19-2.07H12v4.51h6.6c-.29 1.52-1.14 2.82-2.4 3.68v3.05h3.88c2.27-2.09 3.665-5.17 3.665-9.17z" />
              <path fill="#34A853" d="M12 24c3.24 0 5.95-1.08 7.93-2.91l-3.88-3.05c-1.08.72-2.45 1.16-4.05 1.16-3.12 0-5.77-2.1-6.72-4.93H1.25v3.15C3.26 21.36 7.33 24 12 24z" />
              <path fill="#FBBC05" d="M5.28 14.27c-.25-.72-.38-1.49-.38-2.27s.13-1.55.38-2.27V6.58H1.25C.45 8.18 0 9.99 0 12s.45 3.82 1.25 5.42l4.03-3.15z" />
              <path fill="#EA4335" d="M12 4.75c1.77 0 3.35.61 4.6 1.8l3.42-3.42C17.95 1.19 15.24 0 12 0 7.33 0 3.26 2.64 1.25 6.58l4.03 3.15c.95-2.83 3.6-4.98 6.72-4.98z" />
            </svg>
            <span>Google</span>
          </button>
        </div>

        {/* OAuth Info Modal / Card if triggered */}
        {oauthNotice && (
          <div
            style={{
              padding: "14px 16px",
              borderRadius: "8px",
              background: "rgba(30, 41, 59, 0.8)",
              border: "1px solid rgba(56, 189, 248, 0.3)",
              marginBottom: "16px",
              fontSize: "13px",
              lineHeight: 1.5,
              color: "#cbd5e1",
            }}
          >
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "8px" }}>
              <strong style={{ color: "#38bdf8" }}>{oauthNotice.provider} OAuth Integration</strong>
              <button
                type="button"
                onClick={() => setOauthNotice(null)}
                style={{ background: "transparent", border: "none", color: "#94a3b8", cursor: "pointer", fontSize: "16px" }}
              >
                ×
              </button>
            </div>
            <p style={{ margin: "0 0 10px 0" }}>
              To enable production {oauthNotice.provider} Single Sign-On, ensure <code>{oauthNotice.provider === "GitHub" ? "GITHUB_CLIENT_ID" : "GOOGLE_CLIENT_ID"}</code> and secret are set in the server environment.
            </p>
            <div style={{ display: "flex", gap: "8px", flexWrap: "wrap" }}>
              <button
                type="button"
                onClick={() => proceedOAuth(oauthNotice.provider)}
                style={{
                  background: "linear-gradient(135deg, #0284c7 0%, #6366f1 100%)",
                  border: "none",
                  color: "#ffffff",
                  padding: "6px 12px",
                  borderRadius: "6px",
                  fontSize: "12px",
                  fontWeight: 600,
                  cursor: "pointer",
                }}
              >
                Redirect to {oauthNotice.provider} SSO →
              </button>
              <button
                type="button"
                onClick={useDemoCredentials}
                style={{
                  background: "rgba(255, 255, 255, 0.08)",
                  border: "1px solid rgba(255, 255, 255, 0.15)",
                  color: "#f8fafc",
                  padding: "6px 12px",
                  borderRadius: "6px",
                  fontSize: "12px",
                  fontWeight: 500,
                  cursor: "pointer",
                }}
              >
                Auto-fill Dev Account
              </button>
            </div>
          </div>
        )}

        <div style={{ display: "flex", alignItems: "center", gap: "12px", margin: "14px 0" }}>
          <div style={{ flex: 1, height: "1px", background: "rgba(255, 255, 255, 0.08)" }} />
          <span style={{ fontSize: "12px", textTransform: "uppercase", letterSpacing: "0.08em", color: "#64748b" }}>
            or continue with email
          </span>
          <div style={{ flex: 1, height: "1px", background: "rgba(255, 255, 255, 0.08)" }} />
        </div>

        <label className="field">
          <span>Email</span>
          <input
            name="email"
            type="email"
            placeholder="you@example.com"
            autoComplete="email"
            value={email}
            aria-invalid={state === "error"}
            onChange={(event) => setEmail(event.target.value)}
            required
          />
        </label>
        <label className="field">
          <span>Password</span>
          <input
            name="password"
            type="password"
            placeholder={kind === "register" ? "At least 12 characters" : "Your password"}
            autoComplete={kind === "login" ? "current-password" : "new-password"}
            value={password}
            aria-invalid={state === "error"}
            minLength={kind === "register" ? 12 : undefined}
            onChange={(event) => setPassword(event.target.value)}
            required
          />
        </label>
        <button
          className="btn"
          style={{ width: "100%", marginTop: "12px" }}
          type="submit"
          data-state={state === "idle" ? undefined : state}
          disabled={state === "loading"}
        >
          {label}
        </button>
        <p
          className="note"
          data-state={state === "error" || state === "success" ? state : undefined}
          aria-live="polite"
        >
          {message}
        </p>
      </form>
      <p className="auth-footnote">
        {kind === "login" ? (
          <>
            No account yet? <a href="/register">Create an account</a>.
          </>
        ) : (
          <>
            Already have an account? <a href="/login">Sign in</a>.
          </>
        )}
      </p>
    </div>
  );
}
