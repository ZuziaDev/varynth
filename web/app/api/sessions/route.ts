import { NextRequest, NextResponse } from "next/server";
import { readSessionToken } from "../../auth-cookie";

const API_URL = process.env.API_URL ?? "http://127.0.0.1:8080";

export async function GET(request: NextRequest) {
  const token = readSessionToken((name) => request.cookies.get(name)?.value);
  if (!token) {
    return NextResponse.json(
      { error: { message: "Sign in to view CLI sessions.", type: "auth" } },
      { status: 401 },
    );
  }
  try {
    const upstream = await fetch(`${API_URL}/v1/me/sessions`, {
      headers: {
        accept: "application/json",
        authorization: `Bearer ${token}`,
      },
      cache: "no-store",
      signal: AbortSignal.timeout(8000),
    });
    const body = await upstream.json().catch(() => ({}));
    return NextResponse.json(body, { status: upstream.status });
  } catch {
    return NextResponse.json(
      { error: { message: "The API did not answer.", type: "upstream" } },
      { status: 502 },
    );
  }
}

export async function POST(request: NextRequest) {
  const token = readSessionToken((name) => request.cookies.get(name)?.value);
  if (!token) {
    return NextResponse.json(
      { error: { message: "Sign in to manage CLI sessions.", type: "auth" } },
      { status: 401 },
    );
  }

  const payload = (await request.json().catch(() => ({}))) as {
    action?: string;
    session_key?: string;
    title?: string;
    model?: string;
    workspace?: string;
    message_count?: number;
    preview?: string;
  };

  const isDelete = payload.action === "delete";
  const targetPath = isDelete ? "/v1/me/sessions/delete" : "/v1/me/sessions";

  try {
    const upstream = await fetch(`${API_URL}${targetPath}`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        accept: "application/json",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify(payload),
      cache: "no-store",
      signal: AbortSignal.timeout(8000),
    });
    const body = await upstream.json().catch(() => ({}));
    return NextResponse.json(body, { status: upstream.status });
  } catch {
    return NextResponse.json(
      { error: { message: "The API did not answer.", type: "upstream" } },
      { status: 502 },
    );
  }
}
