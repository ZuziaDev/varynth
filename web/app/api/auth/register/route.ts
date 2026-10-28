import { NextResponse } from "next/server";

export async function POST(request: Request) {
  const payload = await request.json();
  const upstream = await fetch(`${process.env.API_URL ?? "http://127.0.0.1:8080"}/v1/auth/register`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  const body = await upstream.json().catch(() => ({ error: { message: "register failed" } }));
  return NextResponse.json(body, { status: upstream.status });
}
