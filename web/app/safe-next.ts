const PLACEHOLDER_ORIGIN = "http://varynth.invalid";

/**
 * Accepts only same-origin, absolute-path redirects. `new URL` normalizes
 * backslashes and scheme-relative prefixes the way browsers do, so
 * `//evil.com`, `/\evil.com` and `https://evil.com` all resolve to a foreign
 * origin and are rejected.
 */
export function safeNextPath(
  raw: string | null | undefined,
  origin: string = PLACEHOLDER_ORIGIN,
): string | null {
  if (!raw) return null;
  let url: URL;
  try {
    url = new URL(raw, origin);
  } catch {
    return null;
  }
  if (url.origin !== origin) return null;
  if (!url.pathname.startsWith("/")) return null;
  if (url.pathname.startsWith("//") || url.pathname.startsWith("/\\")) return null;
  return `${url.pathname}${url.search}${url.hash}`;
}
