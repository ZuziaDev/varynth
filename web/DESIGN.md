---
name: Varynth Console
description: Hallmark Console + Impeccable system. Graphite ground stepped in three lightnesses, pale ink, hairline panels cut from one surface, Host Grotesk for what a person reads, Atkinson Hyperlegible Mono for what an instrument measured, amber for a person and cool blue for an instrument. Zero glow, zero glass, zero purple gradients.

colors:
  paper: "oklch(17% 0.008 250)"
  paper-2: "oklch(21% 0.009 250)"
  paper-3: "oklch(13.5% 0.007 250)"
  ink: "oklch(95% 0.005 250)"
  ink-2: "oklch(83% 0.008 250)"
  muted: "oklch(71% 0.01 250)"
  rule: "oklch(31% 0.01 250)"
  rule-2: "oklch(42% 0.012 250)"
  accent: "oklch(82% 0.15 80)"
  accent-ink: "oklch(18% 0.04 75)"
  accent-soft: "oklch(29% 0.05 80)"
  focus: "oklch(82% 0.15 80)"
  cool: "oklch(77% 0.12 245)"
  cool-soft: "oklch(29% 0.05 245)"
  ok: "oklch(76% 0.15 150)"
  ok-soft: "oklch(24.5% 0.03 150)"
  bad: "oklch(70% 0.17 25)"
  bad-soft: "oklch(24.5% 0.035 25)"

typography:
  scale:
    "13": "0.8125rem"
    "14": "0.875rem"
    "16": "1rem"
    "20": "1.25rem"
    "25": "1.5625rem"
    "31": "1.953rem"
    "39": "2.441rem"
  display:
    fontFamily: "Host Grotesk, ui-sans-serif, system-ui, sans-serif"
    fontSize: "clamp(2.75rem, 5vw + 1rem, 5.25rem)"
    fontWeight: 500
    letterSpacing: "-0.03em"
    lineHeight: 1.04
  body:
    fontFamily: "Host Grotesk, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 400
    lineHeight: 1.55
  mono:
    fontFamily: "Atkinson Hyperlegible Mono, ui-monospace, monospace"
    fontSize: "0.875rem"
    fontWeight: 400
    letterSpacing: "0"

rounded:
  none: "0"
  chip: "4px"
  pill: "6px"
  card: "10px"

spacing:
  "3xs": "2px"
  "2xs": "4px"
  xs: "8px"
  sm: "12px"
  md: "16px"
  lg: "24px"
  xl: "40px"
  "2xl": "64px"
  "3xl": "96px"
  "4xl": "144px"

components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-ink}"
    rounded: "{rounded.pill}"
    padding: "0 24px"
    height: "44px"
  button-quiet:
    backgroundColor: "{colors.paper-2}"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "0 24px"
    height: "44px"
  input-field:
    backgroundColor: "{colors.paper-3}"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "0 16px"
    height: "44px"
  panel-card:
    backgroundColor: "{colors.paper-2}"
    textColor: "{colors.ink}"
    rounded: "{rounded.card}"
    padding: "24px"
---

# Design System: Varynth Console

## Overview

**Creative North Star: "The Precision Lab Notebook"**

Built on **Hallmark Console** (`console: graphite`) and verified against **Impeccable** and **Hallmark** Floor gates. An instrument kept by careful engineers: a graphite ground stepped in three lightnesses, hairline panels cut from one surface, Host Grotesk for what a person reads, Atkinson Hyperlegible Mono for what an instrument measured, warm amber for a person, and cool signal blue for an instrument. Nothing glows, nothing blurs, and no purple-to-cyan gradients exist.

**Key Characteristics:**
- Three neutral graphite steps (`paper-3`, `paper`, `paper-2`) parted by 1px `--color-rule` hairlines.
- Semantic duality: Amber (`--color-accent`) marks human action; Cool blue (`--color-cool`) marks machine measurements and identifiers.
- Strict anti-slop discipline: zero eyebrows, zero tracked-caps labels, zero emoji icons, zero glassmorphism.

## Colors

A dark neutral graphite ground (`hue 250`, low chroma `0.007–0.012`) so both warm human signals and cool instrument readings sit at equal perceptual weight.

### Primary
- **Operator Amber** (`oklch(82% 0.15 80)`): Marks human action, operator decisions, and the single primary filled button per screen, paired with `--color-accent-ink` (`oklch(18% 0.04 75)`).

### Secondary
- **Instrument Blue** (`oklch(77% 0.12 245)`): Marks machine telemetry, model IDs, CLI commands, version tags, and quota progress bars.

### Neutral
- **Graphite Ground** (`oklch(17% 0.008 250)`): Primary page canvas (`--color-paper`).
- **Raised Panel** (`oklch(21% 0.009 250)`): Cards, rows, and form containers (`--color-paper-2`).
- **Recessed Well** (`oklch(13.5% 0.007 250)`): Code blocks, inputs, and terminal wells (`--color-paper-3`).
- **Pale Ink** (`oklch(95% 0.005 250)`): Primary headings and body copy (`--color-ink`).
- **Muted Slate** (`oklch(71% 0.01 250)`): Secondary prose, captions, and high-contrast input borders (`--color-muted`).

### Named Rules
**The Two-Signal Rule.** Amber is a person; cool blue is an instrument. Never swap their meanings or introduce a third decorative accent hue.

## Typography

**Display Font:** Host Grotesk (with `ui-sans-serif, system-ui, sans-serif`)
**Body Font:** Host Grotesk
**Label/Mono Font:** Atkinson Hyperlegible Mono (with `ui-monospace, monospace`)

**Character:** Calm, geometric grotesk reading voice paired with a high-legibility technical monospace whose slashed zero and tabular numerals anchor every measurement.

### Hierarchy
- **Display** (`500`, `clamp(2.75rem, 5vw + 1rem, 5.25rem)`, `1.04`): Hero headlines (`h1`), sentence case, `-0.03em` tracking.
- **Headline** (`500`, `clamp(2rem, 2.5vw + 1rem, 3.5rem)`, `1.04`): Section headings (`h2`).
- **Title** (`500`, `clamp(1.25rem, 0.75vw + 0.875rem, 1.625rem)`, `1.2`): Card and subsystem titles (`h3`).
- **Body** (`400`, `1rem`, `1.55`): Primary prose capped at `65ch`.
- **Label** (`500`, `0.875rem`, `0em`, sentence case): Form labels, table headers, and status chips.

### Named Rules
**The Untracked Label Rule.** Labels, table headers, tags, and nav links are set in the body face at a smaller step in sentence or lower case, never uppercase with letter-spacing.

## Layout

Centered `.shell` container capped at `1440px` with `64px` desktop gutters (`24px` tablet, `16px` mobile). Sections part by `96px` (`--space-3xl`) vertical padding and `1px` `--color-rule` borders. Multi-panel bentos and skill matrices use `1px` hairline grid seams (`gap: var(--rule-hair)` over `background: var(--color-rule)`).

## Elevation & Depth

Tonal layering across three lightness steps (`13.5%`, `17%`, `21%`) and `1px` solid rules (`--color-rule`, `--color-rule-2`). `--shadow-card: none`. No drop shadows, radial blooms, or backdrop blurs are used anywhere.

## Shapes

Crisp technical geometry: `10px` (`--radius-card`) on panels, tables, and bentos; `6px` (`--radius-pill`) on buttons, inputs, and code snippets; `4px` (`--radius-chip`) on status tags and progress meters.

## Components

### Buttons
- **Shape:** Pill-cornered (`6px` radius), `44px` minimum height, `nowrap` labels.
- **Primary (`.btn`):** Solid `--color-accent` fill with `--color-accent-ink` text; shifts to `--color-accent-hover` on hover.
- **Secondary (`.btn-quiet`, `.text-link`):** `--color-paper-2` background with `1px solid var(--color-rule-2)` border; border and text turn `--color-accent` on hover.

### Chips (`.tag`)
- **Style:** `4px` radius, `1px` border, sentence/lowercase `0.8125rem` text.
- **State:** `.tag-pro` uses `--color-accent-soft` with `--color-accent`; `.tag-free` uses `--color-cool-soft` with `--color-cool`.

### Cards / Containers (`.bento-card`, `.panel`, `.plan`)
- **Corner Style:** `10px` radius (`--radius-card`).
- **Background:** `--color-paper-2` (or `--color-paper-3` for `.plate` and `.instrument-panel`).
- **Border:** `1px solid var(--color-rule)` or `var(--color-rule-2)`.

### Inputs / Fields (`.field input`)
- **Style:** `44px` height, `--color-paper-3` background, `1px solid var(--color-muted)` border (ensuring >3:1 edge contrast against `--color-paper-2`), `6px` radius.
- **Focus:** Instant `2px solid var(--color-focus)` outline with `2px` offset.
- **Error:** Border shifts to `--color-bad` when `aria-invalid="true"`.

### Navigation (`.nav`)
- **Style:** Edge-aligned top bar (`64px` min-height) over a `1px` bottom rule, pairing the `varynth` wordmark and `v0.1.1` tag on the left with route links and one outlined `.nav-cta` on the right.

## Do's and Don'ts

### Do:
- **Do** route every color and font through `tokens.css` custom properties.
- **Do** pair every background surface change with an explicit text color token.
- **Do** keep input borders at `--color-muted` so field edges maintain >=3:1 contrast.

### Don't:
- **Don't** use eyebrows or kickers above headings.
- **Don't** use tracked uppercase labels (`text-transform: uppercase`).
- **Don't** use emojis (`🟣`, `🎬`, `🔒`) as UI icons or status indicators.
- **Don't** use purple/cyan gradients, glowing orbs, or `backdrop-filter: blur()`.
