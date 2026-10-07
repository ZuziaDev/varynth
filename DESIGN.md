# Varynth Control Room Design

## Purpose

An operational companion to local coding sessions. The transcript is the primary working surface. Session history, runtime inspection and approval decisions remain reachable without marketing sections or competitor comparisons.

## Tokens

| Role | Value |
| --- | --- |
| Workspace | `#101115` |
| Rails | `#15171c` |
| Panels | `#191b21` |
| Raised controls | `#22252d` |
| Seam | `#2e313b` |
| Primary text | `#f0eff5` |
| Secondary text | `#b2b2be` |
| Muted text | `#9699aa` |
| Primary/selection | `#ad91f8` |
| Success | `#9bdebe` |
| Waiting | `#efd38f` |
| Error | `#ffa1ad` |

Geist variable is bundled locally under OFL-1.1. It serves operational text rather than a display or marketing identity. Code, source-line numbers and measurement use the system monospace stack. Fixed-size text is 11-20px; 11px is reserved for compact operational metadata, with 13px conversation body. Letter spacing is zero. Lucide icons are bundled under ISC in one local sprite. The Varynth mark is authored project geometry.

## Composition

At desktop width, a 58px top bar sits above a 236px session rail, flexible transcript and 310px inspector. Hairline seams define surfaces, not decorative card stacks. A composer stays attached to the transcript; Activity and Approvals replace the conversation area without changing the rails.

Below 1170px the inspector becomes a dismissible right panel. Below 720px sessions become a dismissible left panel and the workspace takes full width. Backdrop, Escape and explicit close controls return to the task. No controls depend on hover alone. Drawers preserve readable dimensions instead of shrinking text. Focus rings, selection color and caret colors follow the palette; reduced motion disables drawer transitions.

## State and Data

- Never confuse selected transcript id, active runtime id and captured stream id.
- Raw id-less approval notices are informational. Only normalized relay ids create decisions.
- Keep approval cards until the RPC returns an accepted/expired decision; connection errors keep retryable cards.
- Deduplicate structured and JSON-text approvals and retain a bounded resolved-id set.
- Show provider-reported usage separately from context estimates. Do not invent cost or a context capacity absent from the server contract.
- Keep the token in memory only and send it consistently to REST/SSE/WebSocket paths.
- Render model/session/event/tool strings as text. Code fences are rendered without HTML or Markdown script injection.
- Bounded activity supports type filtering. Skills collapse after eight entries, while all remain available.

## Assets and Packaging

HTML, CSS, JS, font, mark and sprite are embedded into the Rust binary through an exact whitelist. Every referenced asset is explicitly included in `Cargo.toml`. No external font, icon, analytics or CDN request is needed when opening the UI.

## Verification

Frontend logic tests use Node's built-in runner. Rust tests cover embedded bytes, MIME, whitelist rejection, workspace matching and the shared session reset hook. Full GUI evidence is captured at desktop/mobile using an isolated runtime and local mock provider. Package/build verification uses no running executable from `target/debug` or `target/release`; preview instances use copies.
