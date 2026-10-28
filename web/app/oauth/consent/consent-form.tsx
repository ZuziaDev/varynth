"use client";

import { useState } from "react";

export function ConsentForm(props: {
  responseType: string;
  clientId: string;
  redirectUri: string;
  state: string;
  codeChallenge: string;
  codeChallengeMethod: string;
}) {
  const [state, setState] = useState<"idle" | "loading" | "error" | "success">("idle");
  const [message, setMessage] = useState("");

  async function decide(decision: "approve" | "deny") {
    setState("loading");
    setMessage("");
    try {
      const response = await fetch("/api/oauth/decide", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ ...props, decision }),
      });
      const body = (await response.json().catch(() => ({}))) as {
        redirect_to?: string;
        error?: { message?: string };
      };
      if (!response.ok || !body.redirect_to) {
        setState("error");
        setMessage(body.error?.message ?? "Could not finish sign-in.");
        return;
      }
      setState("success");
      window.location.assign(body.redirect_to);
    } catch {
      setState("error");
      setMessage("Network error. Try again.");
    }
  }

  return (
    <div className="auth-wrap">
      <section className="form-card">
        <h1>Allow this terminal</h1>
        <p className="form-subtitle">
          Varynth CLI wants to bind a local session on this machine.
        </p>
        <div className="code-snippet" style={{ marginBottom: "var(--space-md)" }}>
          Client ID: <strong>{props.clientId || "varynth-cli"}</strong>
        </div>
        <div className="hero-actions" style={{ marginTop: 0 }}>
          <button
            className="btn"
            type="button"
            data-state={state === "idle" ? undefined : state}
            disabled={state === "loading"}
            onClick={() => decide("approve")}
          >
            {state === "loading" ? "Working" : state === "success" ? "Approved" : "Approve session"}
          </button>
          <button
            className="btn-quiet"
            type="button"
            disabled={state === "loading"}
            onClick={() => decide("deny")}
          >
            Deny
          </button>
        </div>
        <p
          className="note"
          data-state={state === "error" ? "error" : undefined}
          role={state === "error" ? "alert" : "status"}
        >
          {message}
        </p>
      </section>
    </div>
  );
}
