export const SESSION_COOKIE = "varynth_session";
export const HOST_SESSION_COOKIE = "__Host-varynth_session";

/**
 * The deployment is secure when the request arrived over HTTPS (directly or
 * through a TLS-terminating proxy). Local http development stays insecure so
 * the cookie still works without TLS.
 */
export function isSecureRequest(request: Request): boolean {
  const forwarded = request.headers.get("x-forwarded-proto");
  if (forwarded) {
    return forwarded.split(",")[0].trim().toLowerCase() === "https";
  }
  try {
    return new URL(request.url).protocol === "https:";
  } catch {
    return false;
  }
}

export function sessionCookieName(secure: boolean): string {
  return secure ? HOST_SESSION_COOKIE : SESSION_COOKIE;
}

/**
 * Reads the session from either cookie name so a deployment that switched
 * between http and https keeps signed-in users.
 */
export function readSessionToken(get: (name: string) => string | null | undefined): string | null {
  for (const name of [HOST_SESSION_COOKIE, SESSION_COOKIE]) {
    const token = get(name)?.trim();
    if (token) return token;
  }
  return null;
}

export type SessionCookieOptions = {
  httpOnly: boolean;
  sameSite: "lax";
  path: string;
  maxAge: number;
  secure: boolean;
};

/**
 * `__Host-` requires `Secure`, `Path=/` and no `Domain`; this shape satisfies it.
 */
export function sessionCookieOptions(secure: boolean, maxAge: number): SessionCookieOptions {
  return { httpOnly: true, sameSite: "lax", path: "/", maxAge, secure };
}
