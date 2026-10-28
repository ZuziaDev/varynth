import { NextRequest, NextResponse } from "next/server";
import { readSessionToken } from "../../auth-cookie";

const API_URL = process.env.API_URL ?? "http://127.0.0.1:8080";

export async function GET(request: NextRequest) {
  const token = readSessionToken((name) => request.cookies.get(name)?.value);
  if (!token) {
    return NextResponse.json(
      { error: { message: "Sign in to view usage.", type: "auth" } },
      { status: 401 },
    );
  }
  try {
    const upstream = await fetch(`${API_URL}/v1/me/usage`, {
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
      { error: { message: "Sign in to use the studio.", type: "auth" } },
      { status: 401 },
    );
  }

  const payload = (await request.json().catch(() => ({}))) as {
    mode?: "chat" | "image" | "search";
    model?: unknown;
    prompt?: unknown;
    system?: unknown;
    query?: unknown;
    provider?: unknown;
  };

  const mode = payload.mode ?? "chat";
  const startedAt = Date.now();

  try {
    let upstream: Response;

    if (mode === "image") {
      const prompt = typeof payload.prompt === "string" ? payload.prompt.trim() : "";
      const model =
        typeof payload.model === "string" && payload.model.trim().length > 0
          ? payload.model.trim()
          : "varynth/flux-1-schnell";
      if (!prompt) {
        return NextResponse.json(
          { error: { message: "Image prompt is required.", type: "invalid_request_error" } },
          { status: 400 },
        );
      }
      upstream = await fetch(`${API_URL}/v1/images/generations`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          accept: "application/json",
          authorization: `Bearer ${token}`,
        },
        body: JSON.stringify({ prompt, model, n: 1 }),
        cache: "no-store",
        signal: AbortSignal.timeout(60000),
      });
    } else if (mode === "search") {
      const query = typeof payload.query === "string" ? payload.query.trim() : "";
      const provider =
        typeof payload.provider === "string" && payload.provider.trim().length > 0
          ? payload.provider.trim()
          : "auto";
      if (!query) {
        return NextResponse.json(
          { error: { message: "Search query is required.", type: "invalid_request_error" } },
          { status: 400 },
        );
      }
      upstream = await fetch(`${API_URL}/v1/search`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          accept: "application/json",
          authorization: `Bearer ${token}`,
        },
        body: JSON.stringify({ query, provider }),
        cache: "no-store",
        signal: AbortSignal.timeout(25000),
      });
    } else {
      const model =
        typeof payload.model === "string" && payload.model.trim().length > 0
          ? payload.model.trim()
          : "varynth/gpt-oss-120b";
      const prompt = typeof payload.prompt === "string" ? payload.prompt.trim() : "";
      const system =
        typeof payload.system === "string" && payload.system.trim().length > 0
          ? payload.system.trim()
          : "You are Varynth, a concise, high-precision sovereign coding assistant.";

      if (!prompt) {
        return NextResponse.json(
          { error: { message: "Prompt is required.", type: "invalid_request_error" } },
          { status: 400 },
        );
      }

      upstream = await fetch(`${API_URL}/v1/chat/completions`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          accept: "application/json",
          authorization: `Bearer ${token}`,
        },
        body: JSON.stringify({
          model,
          messages: [
            { role: "system", content: system },
            { role: "user", content: prompt },
          ],
          max_tokens: 512,
          stream: false,
        }),
        cache: "no-store",
        signal: AbortSignal.timeout(45000),
      });
    }

    const latencyMs = Date.now() - startedAt;
    const body = await upstream.json().catch(() => ({}));

    const usageResp = await fetch(`${API_URL}/v1/me/usage`, {
      headers: {
        accept: "application/json",
        authorization: `Bearer ${token}`,
      },
      cache: "no-store",
      signal: AbortSignal.timeout(5000),
    }).catch(() => null);
    const updatedUsage = usageResp && usageResp.ok ? await usageResp.json().catch(() => null) : null;

    return NextResponse.json(
      {
        latency_ms: latencyMs,
        completion: body,
        result: body,
        usage: updatedUsage,
      },
      { status: upstream.status },
    );
  } catch {
    return NextResponse.json(
      {
        error: {
          message: "Upstream gateway timed out or did not answer.",
          type: "upstream",
        },
      },
      { status: 502 },
    );
  }
}
