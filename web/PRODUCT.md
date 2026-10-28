# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users
Software engineers, systems programmers, and AI developers using the Varynth local-first autonomous coding runtime (`varynth` CLI/TUI) and managing their Varynth account catalog, skills entitlements, and rate quotas.

## Product Purpose
Provide a transparent web console for Varynth's local-first Rust coding runtime, exposing the zero-account public model set, the tiered Agent Skills catalog, server-enforced daily and per-minute quotas, and OAuth 2.0 Loopback PKCE authentication for CLI sessions.

## Positioning
Five public frontier models ready on first launch inside a single local Rust binary with zero upstream vendor branding leaks, paired with isolated BYOK custom endpoints whose secrets never leave the user's machine.

## Operating Context
Developers install `@zuziadev/varynth` in their terminal, run `varynth` immediately against public models, or run `varynth login` to approve a loopback PKCE session in the browser and sync their Free or Pro account catalog and skills (`.agents/skills/`).

## Capabilities and Constraints
1. **Landing (`/`):** Explain Varynth's zero-account public model set, local-first Rust runtime (LSP, episodic memory vault, `computer-use` overlay, deterministic 60fps `motion-fream` synthesis), and custom BYOK endpoint isolation.
2. **Catalog (`/models`):** Display the Varynth Public Models (`/v1/models/public`), the Tiered Agent Skills Catalog (`/v1/catalog/skills`), and the authenticated Account Catalog (`/v1/catalog`).
3. **Plans (`/pricing`):** Show transparent, server-enforced daily and per-minute quotas for Anonymous, Free, and Pro tiers (`/v1/plans`), and allow self-service plan changes when billing is enabled.
4. **Workspace (`/dashboard`):** Show authenticated identity (`/v1/auth/me`), active tier, unlocked skills count, and live UTC quota counters (`/v1/me/usage`).
5. **Authentication (`/login`, `/register`, `/oauth/consent`):** Handle account sign-in, registration, and OAuth 2.0 Loopback PKCE consent for the `varynth login` CLI handshake.

## Evidence on Hand
- All model IDs, skill IDs, and quota limits come live from the Rust API (`/v1/models/public`, `/v1/catalog/skills`, `/v1/catalog`, `/v1/plans`, `/v1/me/usage`).
- Custom endpoints added via `varynth endpoint add` keep secrets strictly on the user's machine and never proxy through Varynth.
- Absence: Never invent fake metrics, customer testimonials, or third-party company logos.

## Product Principles
- Local by default: the binary works on first launch without requiring registration or an upstream API key.
- Honest telemetry: every number shown on the web console comes directly from the server that enforces it.
- Strict secret isolation: user-supplied custom endpoints and API keys never route through Varynth servers.
