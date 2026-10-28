import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";

const ROOT = join(import.meta.dirname, "..");

test("tokens.css declares Hallmark Console OKLCH palette and bans stock Tailwind hex colors", () => {
  const tokens = readFileSync(join(ROOT, "tokens.css"), "utf8");
  assert.match(tokens, /--color-paper:\s*oklch\(/);
  assert.match(tokens, /--color-accent:\s*oklch\(/);
  assert.match(tokens, /--color-accent-ink:\s*oklch\(/);
  assert.match(tokens, /--color-cool:\s*oklch\(/);
  assert.ok(!tokens.includes("#8b5cf6"), "must not contain AI slop purple #8b5cf6");
  assert.ok(!tokens.includes("#06b6d4"), "must not contain AI slop cyan #06b6d4");
  assert.ok(!tokens.includes("#f8fafc"), "must not contain stock Tailwind hex #f8fafc");
});

test("globals.css carries Hallmark stamp and has zero color literals outside tokens.css", () => {
  const css = readFileSync(join(ROOT, "app", "globals.css"), "utf8");
  assert.match(css, /^\/\* Hallmark · shape: Audit Trail · genre: modern-minimal · theme: Console/);
  assert.match(css, /\/\* Hallmark · pre-emit critique:/);
  assert.ok(!css.includes("backdrop-filter"), "must not use decorative glassmorphism");
  assert.ok(!css.includes(["-webkit-background-clip", "text"].join(": ")), "must not use gradient text");
  assert.ok(!css.includes("text-transform: uppercase"), "must not use tracked uppercase labels");
  assert.ok(!css.includes("#"), "must not contain hex color literals in globals.css");
  assert.ok(!css.includes("rgba("), "must not contain rgba color literals in globals.css");
  assert.match(
    css,
    /\.field input\s*\{[^}]*border:\s*var\(--rule-hair\)\s*solid\s*var\(--color-muted\)/,
    ".field input must use --color-muted border for >= 3:1 edge contrast against --color-paper-2",
  );
});

test("UI pages contain zero AI slop eyebrows, emojis, or fake window dots", () => {
  const pages = [
    "app/page.tsx",
    "app/layout.tsx",
    "app/models/page.tsx",
    "app/pricing/page.tsx",
    "app/dashboard/page.tsx",
    "app/auth-form.tsx",
    "app/oauth/consent/consent-form.tsx",
  ];
  for (const rel of pages) {
    const src = readFileSync(join(ROOT, rel), "utf8");
    assert.ok(!src.includes('className="eyebrow"'), `${rel} must not contain .eyebrow`);
    assert.ok(!src.includes('className="card-kicker"'), `${rel} must not contain .card-kicker`);
    assert.ok(!src.includes("terminal-dots"), `${rel} must not contain fake window dots`);
    assert.ok(!src.includes("🟣"), `${rel} must not contain emoji icon 🟣`);
    assert.ok(!src.includes("🎬"), `${rel} must not contain emoji icon 🎬`);
    assert.ok(!src.includes("🔒"), `${rel} must not contain emoji icon 🔒`);
  }
});
