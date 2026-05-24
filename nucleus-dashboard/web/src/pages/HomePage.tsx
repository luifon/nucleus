import { Heart, Layers, Compass } from "lucide-react";
import PageShell from "@/components/PageShell";
import Tile from "@/components/Tile";
import { useFetch } from "@/lib/hooks";
import { getHealth } from "@/lib/api";

export default function HomePage() {
  const health = useFetch(getHealth);

  return (
    <PageShell
      title="nucleus-dashboard"
      subtitle={
        <>
          Unified operator app per ADR-015. The dashboard tiles, chat,
          sessions, skills, reminders, diary, vault feed, cron view, and news
          surfaces land in subsequent commits on the{" "}
          <code className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-0.5 text-[var(--color-nucleus-text)]">
            nucleus-dashboard
          </code>{" "}
          feature branch.
        </>
      }
    >
      <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
        <Tile
          Icon={Heart}
          label="api/health"
          status={health.data ? "OK" : health.error ? "DOWN" : "…"}
          statusKind={health.data ? "ok" : health.error ? "down" : "idle"}
        >
          {health.data ? (
            <pre className="overflow-x-auto text-[11px] leading-snug text-[var(--color-nucleus-faint)]">
              {JSON.stringify(health.data, null, 2)}
            </pre>
          ) : health.error ? (
            <div className="text-xs text-[var(--color-status-down)]">{health.error}</div>
          ) : (
            <div className="text-xs text-[var(--color-nucleus-faint)]">fetching…</div>
          )}
        </Tile>

        <Tile Icon={Layers} label="surfaces" status="9" statusKind="idle">
          <div className="text-xs leading-relaxed text-[var(--color-nucleus-faint)]">
            2 scaffolded · 7 pending. Each lands as its own commit on the
            feature branch.
          </div>
        </Tile>

        <Tile Icon={Compass} label="aesthetic" status="LOCKED" statusKind="warn">
          <div className="text-xs leading-relaxed text-[var(--color-nucleus-faint)]">
            JBM mono · near-black · amber accent · hand-rolled components. No
            shadcn, no marketplace theme. See ADR-015 §guardrails.
          </div>
        </Tile>
      </div>

      <section className="mt-10 border-t border-[var(--color-nucleus-border)] pt-6 text-xs text-[var(--color-nucleus-faint)] opacity-80">
        parallel rollout — old dashboard, chat, news/api keep running while
        this app fills in. sunset PR follows once Playwright comparison clears.
      </section>
    </PageShell>
  );
}
