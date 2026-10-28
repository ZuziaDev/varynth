import { NextRequest, NextResponse } from "next/server";
import { readSessionToken } from "../../auth-cookie";

const API_URL = process.env.API_URL ?? "http://127.0.0.1:8080";

export async function POST(request: NextRequest) {
  const payload = (await request.json().catch(() => ({}))) as { plan?: unknown };
  const plan = typeof payload.plan === "string" ? payload.plan.trim().toLowerCase() : "";
  if (plan !== "free" && plan !== "pro") {
    return NextResponse.json(
      { error: { message: "Plan must be free or pro.", type: "invalid_request_error" } },
      { status: 400 },
    );
  }
  const token = readSessionToken((name) => request.cookies.get(name)?.value);
  if (!token) {
    return NextResponse.json(
      { error: { message: "Sign in to change plan.", type: "auth" } },
      { status: 401 },
    );
  }
  try {
    const upstream = await fetch(`${API_URL}/v1/me/subscription`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ plan }),
      cache: "no-store",
      signal: AbortSignal.timeout(8000),
    });
    const body = await upstream.json().catch(() => ({}));
    return NextResponse.json(body, { status: upstream.status });
  } catch {
    return NextResponse.json(
      {
        error: {
          message: "The API did not answer. No plan change was made.",
          type: "upstream",
        },
      },
      { status: 502 },
    );
  }
}
