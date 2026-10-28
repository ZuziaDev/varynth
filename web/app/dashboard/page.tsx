import { redirect } from "next/navigation";
import {
  apiGet,
  ApiKeysResponse,
  CliSessionsResponse,
  Me,
  PublicModel,
  sessionToken,
  SkillsCatalogResponse,
  Usage,
} from "../../lib/api";
import { DashboardConsole } from "../components/DashboardConsole";

export const dynamic = "force-dynamic";

export default async function DashboardPage() {
  const token = await sessionToken();
  if (!token) {
    redirect("/login?next=/dashboard");
  }

  const [
    meResult,
    usageResult,
    plansResult,
    skillsResult,
    modelsResult,
    keysResult,
    sessionsResult,
  ] = await Promise.all([
    apiGet<Me>("/v1/auth/me", { token }),
    apiGet<Usage>("/v1/me/usage", { token }),
    apiGet<{ billing_configured: boolean }>("/v1/plans"),
    apiGet<SkillsCatalogResponse>("/v1/catalog/skills", { token }),
    apiGet<{ data: PublicModel[] }>("/v1/models/public", { token }),
    apiGet<ApiKeysResponse>("/v1/me/keys", { token }),
    apiGet<CliSessionsResponse>("/v1/me/sessions", { token }),
  ]);

  if (!meResult.ok && meResult.status === 401) {
    redirect("/login?next=/dashboard");
  }

  const billing = plansResult.ok ? plansResult.data.billing_configured : false;
  const usage = usageResult.ok ? usageResult.data : null;
  const skills = skillsResult.ok ? skillsResult.data.skills ?? skillsResult.data.data ?? [] : [];
  const models = modelsResult.ok ? modelsResult.data.data : [];
  const initialKeys = keysResult.ok ? keysResult.data.data ?? [] : [];
  const initialSessions = sessionsResult.ok ? sessionsResult.data.data ?? [] : [];
  const gatewayOrigin = process.env.NEXT_PUBLIC_SITE_URL ?? "https://varynth.zuzia.dev";

  return (
    <main className="page">
      <DashboardConsole
        me={meResult.ok ? meResult.data : null}
        meError={meResult.ok ? undefined : meResult.message}
        initialUsage={usage}
        usageError={usageResult.ok ? undefined : usageResult.message}
        billingConfigured={billing}
        skills={skills}
        models={models}
        initialKeys={initialKeys}
        initialSessions={initialSessions}
        sessionJwt={token}
        gatewayOrigin={gatewayOrigin}
      />
    </main>
  );
}
