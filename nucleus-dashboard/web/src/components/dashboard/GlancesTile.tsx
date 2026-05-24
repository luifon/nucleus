import { Link } from "react-router-dom";
import {
  Clock,
  Database,
  BookOpen,
  Newspaper,
  MessageSquare,
  Sparkles,
} from "lucide-react";
import Tile from "@/components/Tile";
import BucketBadge from "@/components/vault/BucketBadge";
import { type Glances } from "@/lib/api";

export default function GlancesTile({ data }: { data: Glances | null }) {
  if (!data) {
    return <Tile label="recent" status="…" statusKind="idle" />;
  }
  return (
    <Tile label="recent" status="OK" statusKind="ok">
      <ul className="space-y-2 text-[12px]">
        {data.next_fire && (
          <Glance
            to={`/reminders`}
            Icon={Clock}
            label="next fire"
            primary={data.next_fire.title_or_body}
            secondary={`${fireTime(data.next_fire.next_fire_at)} · ${data.next_fire.channels ?? ""}`}
          />
        )}
        {data.latest_vault && (
          <Glance
            to={`/vault`}
            Icon={Database}
            label="vault"
            primary={data.latest_vault.relpath}
            secondary={`touched ${relTime(data.latest_vault.mtime_unix * 1000)}`}
            leading={<BucketBadge bucket={data.latest_vault.bucket} />}
          />
        )}
        {data.latest_diary && (
          <Glance
            to={`/diary?agent=${encodeURIComponent(data.latest_diary.agent)}`}
            Icon={BookOpen}
            label="diary"
            primary={data.latest_diary.first_section ?? `${data.latest_diary.agent} · ${data.latest_diary.date}`}
            secondary={`${data.latest_diary.agent} · ${data.latest_diary.date}`}
          />
        )}
        {data.top_news && (
          <Glance
            to={`/news`}
            Icon={Newspaper}
            label="top news"
            primary={data.top_news.title}
            secondary={`${data.top_news.source_name}${data.top_news.notable_score != null ? ` · ${data.top_news.notable_score.toFixed(2)}` : ""}`}
            leading={data.top_news.notable_score != null && data.top_news.notable_score >= 0.7 ? (
              <Sparkles size={10} strokeWidth={1.75} className="text-[var(--color-nucleus-accent)]" />
            ) : null}
          />
        )}
        {data.latest_chat && (
          <Glance
            to={`/chat`}
            Icon={MessageSquare}
            label="last chat"
            primary={data.latest_chat.title ?? `chat ${data.latest_chat.id.slice(0, 8)}`}
            secondary={`active ${relTime(new Date(data.latest_chat.last_active).getTime())}`}
          />
        )}
      </ul>
    </Tile>
  );
}

function Glance({
  to,
  Icon,
  label,
  primary,
  secondary,
  leading,
}: {
  to: string;
  Icon: React.ComponentType<{ size?: number; strokeWidth?: number; className?: string }>;
  label: string;
  primary: string;
  secondary: string;
  leading?: React.ReactNode;
}) {
  return (
    <li>
      <Link
        to={to}
        className="block rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-1.5 text-[var(--color-nucleus-text)] transition-colors hover:border-[var(--color-nucleus-accent)]"
      >
        <div className="flex items-center gap-1.5 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
          <Icon size={10} strokeWidth={1.75} />
          {label}
          {leading && <span className="ml-auto">{leading}</span>}
        </div>
        <div className="mt-0.5 truncate text-[12px]" title={primary}>{primary}</div>
        <div className="text-[10px] text-[var(--color-nucleus-faint)]">{secondary}</div>
      </Link>
    </li>
  );
}

function fireTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  const time = d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  return sameDay ? `today ${time}` : `${d.toLocaleDateString("en-GB", { day: "2-digit", month: "2-digit" })} ${time}`;
}

function relTime(ms: number): string {
  if (!Number.isFinite(ms)) return "—";
  const sec = Math.floor((Date.now() - ms) / 1000);
  if (sec < 60) return `${sec}s ago`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
  if (sec < 86400) return `${Math.floor(sec / 3600)}h ago`;
  return `${Math.floor(sec / 86400)}d ago`;
}
