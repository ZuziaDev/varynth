# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

The serve interface is for developers running local coding sessions. Their immediate task is to choose a session, issue a coding request, inspect what happened, and answer tool approvals. This user scene is inferred from the existing CLI and the requested dashboard redesign.

## Product Purpose

Varynth is a local coding workspace with a TUI, secure tools, MCP, goals and an operator dashboard.

## Operating Context

A Rust binary runs an Axum HTTP/SSE and WebSocket gateway on loopback by default. The browser is a companion to the terminal. Sessions, usage, goals, event frames and approval outcomes come from the running server; the interface must never invent live data.

## Capabilities and Constraints

- Preserve session selection/new session, provider model selection, text streaming, gateway connectivity, ping/status, skills, doctor checks, approvals and diff inspection.
- Preserve Host/Origin validation, token authentication and sandbox policies.
- Build the interface as locally embedded HTML, CSS and JavaScript. No remote font, icon or framework dependency at runtime.
- Mobile layouts must expose the same operations through drawers/tabs.
- `docs/` and the legacy `VARYNTH.md` are retained locally but excluded from future GitHub commits. `AGENTS.md` is the repository instruction contract.

## Brand Commitments

Use the Varynth name and an independent product description. Do not use competitor analogies. The user approved replacing the old panel with a focused Control Room and asked for an improved web UI rather than a cinematic landing page. The purple identity is preserved in restrained accents.

## Evidence on Hand

The existing `src/dashboard.rs`, `src/gateway.rs`, `src/runtime.rs`, `web/diff.html`, and offline tests define product truth. Screenshots shown in the conversation illustrate repository metadata, not a target UI composition.

## Product Principles

- Make the next operation obvious without marketing copy.
- Separate a selected historical transcript from the running session's live state.
- Keep event output bounded and render untrusted content as text.
- Wait for server confirmation before reporting a mutation or approval as successful.
- Fail clearly without leaking tokens or silently changing sessions.
