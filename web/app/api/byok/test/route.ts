import { NextRequest, NextResponse } from "next/server";
import { readSessionToken } from "../../../auth-cookie";

export async function POST(request: NextRequest) {
  const token = readSessionToken((name) => request.cookies.get(name)?.value);
  if (!token) {
    return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
  }

  const body = await request.json().catch(() => ({}));
  const baseUrl = (body.base_url || "").trim().replace(/\/+$/, "");
  const apiKey = (body.api_key || "").trim();

  if (!baseUrl) {
    return NextResponse.json({ error: "Base URL is required" }, { status: 400 });
  }

  // Attempt to fetch models list from the custom endpoint: ${baseUrl}/models
  const modelsUrl = baseUrl.endsWith("/models") ? baseUrl : `${baseUrl}/models`;
  const headers: Record<string, string> = {
    accept: "application/json",
  };
  if (apiKey) {
    headers["authorization"] = `Bearer ${apiKey}`;
  }

  try {
    const res = await fetch(modelsUrl, {
      headers,
      signal: AbortSignal.timeout(7000),
      cache: "no-store",
    });

    if (!res.ok) {
      const errText = await res.text().catch(() => "");
      return NextResponse.json(
        { ok: false, message: `Endpoint returned HTTP ${res.status}: ${errText.slice(0, 200)}` },
        { status: 200 },
      );
    }

    const data = await res.json().catch(() => null);
    let extractedModels: string[] = [];

    if (data && Array.isArray(data.data)) {
      extractedModels = data.data
        .map((m: { id?: string }) => m.id)
        .filter((id: unknown): id is string => typeof id === "string" && id.length > 0);
    } else if (data && Array.isArray(data.models)) {
      extractedModels = data.models
        .map((m: { name?: string; id?: string }) => m.id || m.name)
        .filter((id: unknown): id is string => typeof id === "string" && id.length > 0);
    }

    return NextResponse.json({
      ok: true,
      message: `Connection successful! Found ${extractedModels.length} models.`,
      models: extractedModels,
    });
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : "Connection failed";
    return NextResponse.json({ ok: false, message: `Could not reach endpoint: ${msg}` }, { status: 200 });
  }
}
