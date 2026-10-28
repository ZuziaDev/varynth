import { AuthForm } from "../auth-form";
import { safeNextPath } from "../safe-next";

export default async function RegisterPage({
  searchParams,
}: {
  searchParams: Promise<{ cli?: string; redirect?: string; next?: string }>;
}) {
  const params = await searchParams;
  const next = safeNextPath(params.next);
  const nextHref =
    params.cli === "1" && params.redirect
      ? `/oauth/consent?${params.redirect}`
      : next ?? "/";
  return (
    <main>
      <AuthForm kind="register" nextHref={nextHref} />
    </main>
  );
}
