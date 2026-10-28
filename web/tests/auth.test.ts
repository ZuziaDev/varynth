import assert from "node:assert/strict";
import { test } from "node:test";

import {
  HOST_SESSION_COOKIE,
  SESSION_COOKIE,
  isSecureRequest,
  readSessionToken,
  sessionCookieName,
  sessionCookieOptions,
} from "../app/auth-cookie.ts";
import { safeNextPath } from "../app/safe-next.ts";

test("open redirect: foreign and scheme-relative targets are rejected", () => {
  assert.equal(safeNextPath("/\\evil.com"), null);
  assert.equal(safeNextPath("//evil.com"), null);
  assert.equal(safeNextPath("///evil.com"), null);
  assert.equal(safeNextPath("https://evil.com/steal"), null);
  assert.equal(safeNextPath("http://evil.com"), null);
  assert.equal(safeNextPath("javascript:alert(1)"), null);
  assert.equal(safeNextPath("dashboard"), "/dashboard");
  assert.equal(safeNextPath(""), null);
  assert.equal(safeNextPath(undefined), null);
});

test("open redirect: same-origin relative paths survive", () => {
  assert.equal(safeNextPath("/"), "/");
  assert.equal(safeNextPath("/dashboard?tab=usage"), "/dashboard?tab=usage");
  assert.equal(safeNextPath("/oauth/consent?cli=1#top"), "/oauth/consent?cli=1#top");
  assert.equal(safeNextPath("https://app.varynth.dev/settings", "https://app.varynth.dev"), "/settings");
  assert.equal(
    safeNextPath("https://evil.com/settings", "https://app.varynth.dev"),
    null,
  );
});

test("cookie name follows the request scheme", () => {
  const https = new Request("https://api.varynth.dev/api/auth/login", {
    headers: { "x-forwarded-proto": "https" },
  });
  const http = new Request("http://127.0.0.1:3000/api/auth/login");
  const proxiedTls = new Request("http://127.0.0.1:3000/api/auth/login", {
    headers: { "x-forwarded-proto": "https,http" },
  });

  assert.equal(isSecureRequest(https), true);
  assert.equal(isSecureRequest(http), false);
  assert.equal(isSecureRequest(proxiedTls), true);

  assert.equal(sessionCookieName(true), HOST_SESSION_COOKIE);
  assert.equal(sessionCookieName(false), SESSION_COOKIE);

  const secure = sessionCookieOptions(true, 3600);
  assert.equal(secure.secure, true);
  assert.equal(secure.httpOnly, true);
  assert.equal(secure.path, "/");
  assert.equal(secure.sameSite, "lax");
  assert.equal(secure.maxAge, 3600);

  const local = sessionCookieOptions(false, 3600);
  assert.equal(local.secure, false);
  assert.equal(local.path, "/");
});

test("session reader accepts both cookie names", () => {
  const hostOnly = new Map([[HOST_SESSION_COOKIE, "host-token"]]);
  const plainOnly = new Map([[SESSION_COOKIE, "plain-token"]]);
  const empty = new Map([[SESSION_COOKIE, "  "]]);

  assert.equal(readSessionToken((name) => hostOnly.get(name)), "host-token");
  assert.equal(readSessionToken((name) => plainOnly.get(name)), "plain-token");
  assert.equal(readSessionToken((name) => empty.get(name)), null);
  assert.equal(readSessionToken(() => undefined), null);
});
