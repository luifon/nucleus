import { useEffect, useState } from "react";
import { getHealth, type Health } from "@/lib/api";

export default function HomePage() {
  const [health, setHealth] = useState<Health | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    getHealth().then(setHealth).catch((e) => setErr(String(e)));
  }, []);

  return (
    <div className="p-6">
      <div className="mb-4 text-xs text-[var(--color-nucleus-faint)]">
        ┌── scaffold ──────────────────────────────
      </div>

      <h1 className="mb-2 text-base">
        nucleus-dashboard <span className="text-[var(--color-nucleus-faint)]">/ home</span>
      </h1>

      <p className="mb-6 max-w-2xl text-xs text-[var(--color-nucleus-faint)]">
        Unified operator app per ADR-015. This page is the scaffold cut — the
        dashboard tiles, chat, sessions, skills, reminders, diary, vault feed,
        cron view, and news surfaces land in subsequent commits on the
        <code className="mx-1 border border-[var(--color-nucleus-border)] px-1">nucleus-dashboard</code>
        feature branch. Verify the backend is reachable and the aesthetic
        baseline reads right before scope grows.
      </p>

      <section className="border border-[var(--color-nucleus-border)] p-3 text-xs">
        <div className="mb-2 flex items-center gap-2">
          <span className="text-[var(--color-nucleus-faint)]">api/health</span>
          {health ? (
            <span className="text-[var(--color-status-ok)]">[OK]</span>
          ) : err ? (
            <span className="text-[var(--color-status-down)]">[DOWN]</span>
          ) : (
            <span className="text-[var(--color-nucleus-faint)]">[…]</span>
          )}
        </div>
        <pre className="overflow-x-auto text-[var(--color-nucleus-faint)]">
          {health ? JSON.stringify(health, null, 2) : err ? `error: ${err}` : "fetching…"}
        </pre>
      </section>
    </div>
  );
}
