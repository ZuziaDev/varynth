import { cookies } from "next/headers";
import { NextResponse } from "next/server";
import { readSessionToken } from "../../../auth-cookie";

export async function POST(request: Request) {
  const payload = await request.json();
  const jar = await cookies();
  const session = readSessionToken((name) => jar.get(name)?.value);
  if (!session) {
    return NextResponse.json({ error: { message: "sign in first" } }, { status: 401 });
  }
  const upstream = await fetch(`${process.env.API_URL ?? "http://127.0.0.1:8080"}/v1/oauth/authorize`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${session}`,
    },
    body: JSON.stringify({
      response_type: payload.responseType,
      client_id: payload.clientId,
      redirect_uri: payload.redirectUri,
      state: payload.state,
      code_challenge: payload.codeChallenge,
      code_challenge_method: payload.codeChallengeMethod,
      decision: payload.decision,
    }),
  });
  const body = await upstream.json().catch(() => ({ error: { message: "authorize failed" } }));
  return NextResponse.json(body, { status: upstream.status });
}
