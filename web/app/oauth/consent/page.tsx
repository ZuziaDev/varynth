import { ConsentForm } from "./consent-form";

export default async function ConsentPage({
  searchParams,
}: {
  searchParams: Promise<Record<string, string | string[] | undefined>>;
}) {
  const params = await searchParams;
  const read = (key: string) => {
    const value = params[key];
    return Array.isArray(value) ? value[0] ?? "" : value ?? "";
  };
  return (
    <main>
      <ConsentForm
        responseType={read("response_type")}
        clientId={read("client_id")}
        redirectUri={read("redirect_uri")}
        state={read("state")}
        codeChallenge={read("code_challenge")}
        codeChallengeMethod={read("code_challenge_method")}
      />
    </main>
  );
}
