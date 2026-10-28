import { cookies } from "next/headers";
import { readSessionToken } from "../app/auth-cookie";

export const API_URL = process.env.API_URL ?? "http://127.0.0.1:8080";

export type LimitSet = {
  chat_per_day: number;
  chat_per_minute: number;
  search_per_day: number;
};

export type Plan = {
  id: string;
  name: string;
  price_per_month: number | null;
  quota: LimitSet;
  features: string[];
};

export type PlansResponse = {
  object: string;
  billing_configured: boolean;
  data: Plan[];
  anonymous: LimitSet;
};

export type Usage = {
  plan: string;
  limits: LimitSet;
  today: { day: string; chat: number; search: number; chat_this_minute: number };
  remaining: { chat: number; search: number; chat_this_minute: number };
};

export type Me = { id: string; email: string };

export type PublicModel = { id: string; display_name: string; owned_by: string };

export type CatalogProvider = {
  id: string;
  name: string;
  kind: string;
  models: { id: string; name: string }[];
};

export type SkillItem = {
  id: string;
  name: string;
  description: string;
  tier: "free" | "pro";
  unlocked: boolean;
  content: string | null;
};

export type SkillsCatalogResponse = {
  object: string;
  tier: string;
  skills: SkillItem[];
  data: SkillItem[];
};

export type ApiKeyItem = {
  id: string;
  name: string;
  key_prefix: string;
  created_at: string;
  last_used_at: string | null;
  revoked: boolean;
};

export type ApiKeysResponse = {
  object: string;
  data: ApiKeyItem[];
};

export type CliSessionItem = {
  id: string;
  session_key: string;
  title: string;
  model: string;
  workspace: string;
  message_count: number;
  preview: string;
  updated_at: string;
};

export type CliSessionsResponse = {
  object: string;
  data: CliSessionItem[];
};

export type Result<T> =
  | { ok: true; data: T }
  | { ok: false; status: number; message: string };

export async function sessionToken(): Promise<string | null> {
  const store = await cookies();
  return readSessionToken((name) => store.get(name)?.value);
}

type FetchOptions = { token?: string | null; timeoutMs?: number };

export async function apiGet<T>(path: string, options: FetchOptions = {}): Promise<Result<T>> {
  const requestHeaders: Record<string, string> = { accept: "application/json" };
  if (options.token) {
    requestHeaders.authorization = `Bearer ${options.token}`;
  }
  try {
    const response = await fetch(`${API_URL}${path}`, {
      headers: requestHeaders,
      cache: "no-store",
      signal: AbortSignal.timeout(options.timeoutMs ?? 8000),
    });
    if (!response.ok) {
      return { ok: false, status: response.status, message: await errorMessage(response) };
    }
    return { ok: true, data: (await response.json()) as T };
  } catch {
    return { ok: false, status: 0, message: "The API did not answer. Check that varynth-api is running." };
  }
}

export async function errorMessage(response: Response): Promise<string> {
  const body = (await response.json().catch(() => null)) as
    | { error?: { message?: string } }
    | null;
  if (body?.error?.message) {
    return body.error.message;
  }
  return `The API answered ${response.status}.`;
}
