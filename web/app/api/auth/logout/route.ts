import { NextRequest, NextResponse } from "next/server";
import {
  SESSION_COOKIE,
  HOST_SESSION_COOKIE,
  isSecureRequest,
  sessionCookieName,
  sessionCookieOptions,
} from "../../../auth-cookie";

const API_URL = process.env.API_URL ?? "http://127.0.0.1:8080";

export async function POST(request: NextRequest) {
  const secure = isSecureRequest(request);
  const name = sessionCookieName(secure);
  const token = request.cookies.get(name)?.value ?? request.cookies.get(SESSION_COOKIE)?.value;
  if (token) {
    await fetch(`${API_URL}/v1/auth/logout`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      cache: "no-store",
      signal: AbortSignal.timeout(5000),
    }).catch(() => undefined);
  }
  const response = NextResponse.json({ ok: true });
  for (const cookie of [SESSION_COOKIE, HOST_SESSION_COOKIE]) {
    response.cookies.set(cookie, "", sessionCookieOptions(secure, 0));
  }
  return response;
}
