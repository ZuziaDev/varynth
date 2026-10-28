import { NextResponse } from "next/server";
import {
  SESSION_COOKIE,
  HOST_SESSION_COOKIE,
  isSecureRequest,
  sessionCookieName,
  sessionCookieOptions,
} from "../../../auth-cookie";

export async function POST(request: Request) {
  const payload = await request.json();
  const upstream = await fetch(`${process.env.API_URL ?? "http://127.0.0.1:8080"}/v1/auth/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  const body = await upstream.json().catch(() => ({ error: { message: "login failed" } }));
  if (!upstream.ok) {
    return NextResponse.json(body, { status: upstream.status });
  }
  const secure = isSecureRequest(request);
  const response = NextResponse.json({ email: body.email ?? "" });
  response.cookies.set(
    sessionCookieName(secure),
    String(body.session_token ?? ""),
    sessionCookieOptions(secure, Number(body.expires_in ?? 0)),
  );
  // Drop the other name so a cookie issued before a flag change cannot linger.
  for (const name of [SESSION_COOKIE, HOST_SESSION_COOKIE]) {
    if (name !== sessionCookieName(secure)) {
      response.cookies.set(name, "", sessionCookieOptions(secure, 0));
    }
  }
  return response;
}
