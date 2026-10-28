"use client";

import { useState } from "react";

export function SignOutButton() {
  const [busy, setBusy] = useState(false);

  async function signOut() {
    setBusy(true);
    await fetch("/api/auth/logout", { method: "POST" }).catch(() => undefined);
    window.location.assign("/");
  }

  return (
    <button className="btn btn-quiet" type="button" onClick={signOut} disabled={busy}>
      {busy ? "Signing out" : "Sign out"}
    </button>
  );
}
