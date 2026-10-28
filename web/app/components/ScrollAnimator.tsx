"use client";

import React, { useEffect, useState } from "react";

export function ScrollAnimator() {
  const [progress, setProgress] = useState(0);

  useEffect(() => {
    const update = () => {
      const max = Math.max(
        document.documentElement.scrollHeight - window.innerHeight,
        1
      );
      const pct = Math.min(Math.max((window.scrollY / max) * 100, 0), 100);
      setProgress(pct);
      document.documentElement.style.setProperty("--scroll-pct", `${pct.toFixed(2)}%`);
      document.documentElement.style.setProperty("--scroll-ratio", `${(pct / 100).toFixed(4)}`);
    };

    const observer = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          if (entry.isIntersecting) {
            entry.target.classList.add("is-visible");
          }
        });
      },
      { threshold: 0.1, rootMargin: "0px 0px -40px 0px" }
    );

    const observeAll = () => {
      const targets = document.querySelectorAll(
        ".bento-card, .stage, .plate, .section, .plan, .skill-card, .group, .install-showcase"
      );
      targets.forEach((el) => {
        el.classList.add("scroll-reveal");
        observer.observe(el);
      });
    };

    update();
    observeAll();

    // Re-scan on route transitions
    const mut = new MutationObserver(() => observeAll());
    mut.observe(document.body, { childList: true, subtree: true });

    window.addEventListener("scroll", update, { passive: true });
    window.addEventListener("resize", update);
    return () => {
      window.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
      observer.disconnect();
      mut.disconnect();
    };
  }, []);

  return (
    <div className="scroll-progress-track" aria-hidden="true">
      <div
        className="scroll-progress-bar"
        style={{ width: `${progress}%` }}
      />
    </div>
  );
}
