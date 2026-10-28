import { AuthForm } from "../auth-form";
import { safeNextPath } from "../safe-next";

export default async function LoginPage({
  searchParams,
}: {
  searchParams: Promise<{ cli?: string; redirect?: string; next?: string; error?: string }>;
}) {
  const params = await searchParams;
  const next = safeNextPath(params.next);
  const nextHref = params.cli === "1" && params.redirect
    ? `/oauth/consent?${params.redirect}`
    : next ?? "/";
  return (
    <main>
      <AuthForm kind="login" nextHref={nextHref} initialError={params.error} />
    </main>
  );
}
