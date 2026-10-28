"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";

type PlanId = "free" | "pro";

export function PlanActions({
  currentPlan,
  target,
  label,
}: {
  currentPlan: string;
  target: PlanId;
  label: string;
}) {
  const router = useRouter();
  const [state, setState] = useState<"idle" | "loading" | "error" | "success">("idle");
  const [message, setMessage] = useState("");
  const isCurrent = currentPlan === target;

  async function changePlan() {
    setState("loading");
    setMessage("");
    try {
      const response = await fetch("/api/subscription", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ plan: target }),
      });
      const body = (await response.json().catch(() => ({}))) as {
        plan?: string;
        changed?: boolean;
        error?: { message?: string };
      };
      if (!response.ok) {
        setState("error");
        setMessage(body.error?.message ?? "Could not change the plan.");
        return;
      }
      setState("success");
      setMessage(body.changed ? `Now on ${body.plan}.` : "That is already your plan.");
      router.refresh();
    } catch {
      setState("error");
      setMessage("Network error. No plan change was made.");
    }
  }

  return (
    <div className="plan-action">
      <button
        className="btn"
        type="button"
        onClick={changePlan}
        disabled={isCurrent || state === "loading"}
        data-state={state === "idle" ? undefined : state}
      >
        {isCurrent
          ? "Current plan"
          : state === "loading"
            ? "Working"
            : state === "success"
              ? "Changed"
              : label}
      </button>
      <p
        className="note"
        data-state={state === "error" || state === "success" ? state : undefined}
        role={state === "error" ? "alert" : "status"}
      >
        {message}
      </p>
    </div>
  );
}
