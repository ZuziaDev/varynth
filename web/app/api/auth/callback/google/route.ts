import { NextResponse } from "next/server";
import {
  isSecureRequest,
  sessionCookieName,
  sessionCookieOptions,
  SESSION_COOKIE,
  HOST_SESSION_COOKIE,
} from "../../../../auth-cookie";

function getPublicOrigin(request: Request): string {
  const host = request.headers.get("x-forwarded-host") || request.headers.get("host");
  const proto = request.headers.get("x-forwarded-proto") || "https";

  if (host && !host.includes("localhost") && !host.includes("127.0.0.1") && !host.includes(":3000")) {
    return `${proto}://${host}`;
  }

  return process.env.NEXT_PUBLIC_SITE_URL?.replace(/\/+$/, "") || "https://varynth.zuzia.dev";
}

export async function GET(request: Request) {
  const origin = getPublicOrigin(request);
  const url = new URL(request.url);
  const code = url.searchParams.get("code");
  const error = url.searchParams.get("error");
  const state = url.searchParams.get("state");

  if (error || !code) {
    return NextResponse.redirect(
      new URL(`/login?error=${encodeURIComponent(error ?? "oauth_cancelled")}`, origin),
    );
  }

  const redirectUri = `${origin}/api/auth/callback/google`;
  const apiUrl = process.env.API_URL ?? "http://127.0.0.1:8080";

  try {
    const upstream = await fetch(`${apiUrl}/v1/auth/oauth/google`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        code,
        redirect_uri: redirectUri,
      }),
    });

    if (!upstream.ok) {
      const errBody = await upstream.json().catch(() => ({}));
      const msg = errBody?.error?.message ?? "oauth_failed";
      return NextResponse.redirect(
        new URL(`/login?error=${encodeURIComponent(msg)}`, origin),
      );
    }

    const body = await upstream.json();
    const secure = isSecureRequest(request);
    const target = state && state.startsWith("/") ? state : "/dashboard";
    const response = NextResponse.redirect(new URL(target, origin));

    response.cookies.set(
      sessionCookieName(secure),
      String(body.session_token ?? ""),
      sessionCookieOptions(secure, Number(body.expires_in ?? 86400 * 30)),
    );

    for (const name of [SESSION_COOKIE, HOST_SESSION_COOKIE]) {
      if (name !== sessionCookieName(secure)) {
        response.cookies.set(name, "", sessionCookieOptions(secure, 0));
      }
    }

    return response;
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : "network_error";
    return NextResponse.redirect(
      new URL(`/login?error=${encodeURIComponent(msg)}`, origin),
    );
  }
}
