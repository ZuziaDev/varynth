-- 004_skills_catalog.sql
-- Varynth Tiered Skills Catalog (Free vs. Pro)

CREATE TABLE IF NOT EXISTS skills_catalog (
    id text PRIMARY KEY,
    name text NOT NULL,
    description text NOT NULL,
    tier text NOT NULL CHECK (tier IN ('free', 'pro')),
    content text NOT NULL,
    sort_order integer NOT NULL DEFAULT 0,
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS skills_catalog_tier_sort_idx
    ON skills_catalog (tier, sort_order);

-- Seed Free Tier Skills
INSERT INTO skills_catalog (id, name, description, tier, content, sort_order, enabled)
VALUES
(
    'create-skill',
    'Create Skill',
    'Scaffold and develop new agent skills with standard YAML frontmatter, deterministic execution, and unit tests.',
    'free',
    $$---
name: create-skill
description: Scaffold and develop new agent skills with standard YAML frontmatter, deterministic execution, and unit tests.
---

# Create Skill — Agent Capability Scaffolding

This skill guides the authoring, structure, and verification of new Varynth agent skills.

## Structure
Every skill resides in `.agents/skills/<skill-name>/` and must contain:
1. `SKILL.md`: Root specification with valid YAML frontmatter (`name`, `description`).
2. Optional `scripts/`: Executable helper tools and automation scripts.
3. Optional `examples/`: Sample configurations and reference templates.
4. Optional `references/`: Extended technical documentation.

## Best Practices
- Define deterministic protocols with clear inputs, transitions, and outputs.
- Keep frontmatter descriptions precise and searchable.
- Include failure modes and graceful error recovery routines.
$$,
    10,
    true
),
(
    'cli-design',
    'CLI Design',
    'Guidelines and ergonomic patterns for building professional, responsive terminal user interfaces and command-line tools.',
    'free',
    $$---
name: cli-design
description: Guidelines and ergonomic patterns for building professional, responsive terminal user interfaces and command-line tools.
---

# CLI Design — Ergonomic Terminal Protocols

## Principles
1. **100% Strict English**: Zero localized or untranslated strings across messages, errors, and dialogs.
2. **Visual Hierarchy**: Clear separation of status, interactive forms, and operational logs.
3. **Non-blocking Operations**: Provide immediate progress feedback and background polling for operations > 200ms.
4. **Resilient Inputs**: Graceful keyboard shortcuts (Esc, Ctrl+C handling, and interactive arrow navigation).
$$,
    20,
    true
),
(
    'backend-core',
    'Backend Core',
    'Core backend architecture patterns, asynchronous runtime management, database connection pooling, and resilient API design.',
    'free',
    $$---
name: backend-core
description: Core backend architecture patterns, asynchronous runtime management, database connection pooling, and resilient API design.
---

# Backend Core — Robust Systems Architecture

## Guidelines
1. **Async Runtime**: Built on Tokio multi-thread runtime with structured task spawning and cancellation tokens.
2. **Database Pooling**: Managed PostgreSQL connection pools via SQLx with compile-time checked migrations.
3. **Structured Errors**: Unified `ApiError` mapping domain failures to clean RFC 7807 problem details or OpenAI error shapes.
4. **Observability**: Zero-overhead tracing spans with subscriber level filters and structured JSON logs.
$$,
    30,
    true
),
(
    'ui-foundations',
    'UI Foundations',
    'Foundational design system and styling standards for clean, high-performance web and desktop user interfaces.',
    'free',
    $$---
name: ui-foundations
description: Foundational design system and styling standards for clean, high-performance web and desktop user interfaces.
---

# UI Foundations — Design Tokens & Architecture

## Core Design Tokens
- **Palette**: Obsidian deep base (`#050508`), slate surface (`#0F111A`), violet accent (`#8B5CF6`), active cyan (`#06B6D4`).
- **Typography**: Inter / system font with tight tabular numerals and micro letter-spacing.
- **Glassmorphism**: Backdrop blur with specular 1px border gradients (`linear-gradient(90deg, transparent, rgba(255,255,255,0.2), transparent)`).
$$,
    40,
    true
),
(
    'computer-use',
    'Computer Use',
    'Codex-style visual overlay computer-use operator with DPI scaling, multi-monitor awareness, glowing aim halo, click ripple, live operator pill, and safety kill-switch.',
    'free',
    $$---
name: computer-use
description: Codex-style visual overlay computer-use operator with DPI scaling, multi-monitor awareness, glowing aim halo, click ripple, live operator pill, and safety kill-switch.
---

# Codex-Style Visual Overlay computer-use Operator

The `computer-use` skill equips Varynth with high-precision, safety-guarded GUI automation across native Windows desktop environments, macOS, and Linux.

## Safety & Kill-Switch
- **Escape Key (`Esc`)**: Low-level global keyboard hook immediately aborts automation.
- **Real User Mouse Override**: Physical mouse displacement instantly yields control.

## Visual Overlay
- Topmost click-through transparent window (`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST`).
- Aim Halo reticle snapping to target coordinates.
- Expanding click ripples (Blue `#3B82F6` for left click, Orange `#F97316` for right click).
- Live operator status pill: `🟣 Varynth Operator Active: [x: 420, y: 380]`.
$$,
    50,
    true
),
(
    'motion-fream',
    'Motion Fream',
    'Deterministic frame-by-frame HTML/CSS/Canvas motion graphics & 60fps launch video generator (Playwright + FFmpeg).',
    'free',
    $$---
name: motion-fream
description: Deterministic frame-by-frame HTML/CSS/Canvas motion graphics & 60fps launch video generator (Playwright + FFmpeg).
---

# motion-fream — Deterministic Frame-by-Frame HTML Motion Engine

## Architecture
- Deterministic time representation: every frame `f` is an exact mathematical function of `tSec = frame / fps`.
- Playwright virtual-clock frame stepper pipes lossless PNG frames directly into FFmpeg `image2pipe`.
- Smooth Bezier easing, Apple CASpringAnimation harmonic oscillator curves, and 3D camera transforms.
$$,
    60,
    true
)
ON CONFLICT (id) DO NOTHING;

-- Seed Pro Tier Skills
INSERT INTO skills_catalog (id, name, description, tier, content, sort_order, enabled)
VALUES
(
    'create-mcp',
    'Create MCP',
    'Scaffold and build Model Context Protocol (MCP) servers and tools for Varynth.',
    'pro',
    $$---
name: create-mcp
description: Scaffold and build Model Context Protocol (MCP) servers and tools for Varynth.
---

# Create MCP — Model Context Protocol Engineering (Pro)

## Architecture
- Implement Model Context Protocol specifications (stdio & SSE transport).
- Generate JSON-RPC 2.0 schemas for tools, prompts, and resources.
- Sandboxed IPC process isolation and asynchronous cancellation handling.
$$,
    110,
    true
),
(
    'create-plugin',
    'Create Plugin',
    'Design, package, and distribute native Varynth extensions and plugins.',
    'pro',
    $$---
name: create-plugin
description: Design, package, and distribute native Varynth extensions and plugins.
---

# Create Plugin — Native Varynth Extension Framework (Pro)

## Lifecycle Hooks
- `on_session_start`: Intercept session initialization and configure memory contexts.
- `on_tool_call`: Gate, transform, or enrich tool parameters dynamically.
- `on_turn_complete`: Audit turns and persist episodic memory records.
$$,
    120,
    true
),
(
    'fullstack-architect',
    'Fullstack Architect',
    'End-to-end fullstack systems architecture, zero-downtime migrations, and distributed scalability.',
    'pro',
    $$---
name: fullstack-architect
description: End-to-end fullstack systems architecture, zero-downtime migrations, and distributed scalability.
---

# Fullstack Architect — High-Concurrency Distributed Systems (Pro)

## Key Disciplines
- Zero-downtime multi-phase database migrations (Expand-Contract pattern).
- CQRS and event-driven architectures with idempotent message delivery.
- Edge caching strategies and multi-region distributed consistency.
$$,
    130,
    true
),
(
    'generative-ui-pro',
    'Generative UI Pro',
    'Next-gen generative UI with dynamic streaming component synthesis and reactive state binding.',
    'pro',
    $$---
name: generative-ui-pro
description: Next-gen generative UI with dynamic streaming component synthesis and reactive state binding.
---

# Generative UI Pro — Dynamic Streaming Components (Pro)

## Specifications
- Progressive token streaming into live reactive component virtual DOM trees.
- Isolated sandboxing with secure iframe and shadow DOM boundary containment.
- Bidirectional state synchronization between agent runtime and client UI.
$$,
    140,
    true
),
(
    'motion-fream-pro',
    'Motion Fream Pro',
    'Pro automated deterministic 60fps motion graphics & launch video studio with Flux AI hero asset generation and Playwright + FFmpeg rendering.',
    'pro',
    $$---
name: motion-fream-pro
description: Pro automated deterministic 60fps motion graphics & launch video studio with Flux AI hero asset generation and Playwright + FFmpeg rendering.
---

# motion-fream-pro — Automated AI Flux + Deterministic Video Studio (Pro)

## Automated Workflow
1. Synthesize photorealistic 3D hero assets via text-to-image `generate_image(prompt, output_path, size)`.
2. Construct deterministic HTML5/Canvas scene using exact Newton-Raphson Bezier math and spring dynamics.
3. Headless virtual-clock capture via Playwright stepping frame by frame.
4. Lossless FFmpeg encoding into 60fps H.264 / ProRes video.
$$,
    150,
    true
),
(
    'security-auditor',
    'Security Auditor',
    'Advanced static analysis, secret scanning, dependency supply chain audit, and memory safety verification.',
    'pro',
    $$---
name: security-auditor
description: Advanced static analysis, secret scanning, dependency supply chain audit, and memory safety verification.
---

# Security Auditor — Zero-Trust Code Verification (Pro)

## Audit Protocols
- Unsafe Rust pointer verification, lifetime constraints, and undefined behavior checks.
- High-entropy cryptographic token and API key detection in commit histories.
- Dependency supply chain audit, known CVE checks, and license compliance verification.
$$,
    160,
    true
)
ON CONFLICT (id) DO NOTHING;
