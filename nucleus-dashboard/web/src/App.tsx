import { Routes, Route, NavLink, useLocation } from "react-router-dom";
import {
  LayoutDashboard,
  MessageSquare,
  Terminal,
  Sparkles,
  Bell,
  BookOpen,
  Database,
  Timer,
  Newspaper,
  Activity,
  type LucideIcon,
} from "lucide-react";
import HomePage from "./pages/HomePage";
import NewsPage from "./pages/NewsPage";
import CronPage from "./pages/CronPage";
import SkillsPage from "./pages/SkillsPage";

type RouteEntry = {
  path: string;
  label: string;
  icon: LucideIcon;
  group: "primary" | "observability";
  impl: "scaffolded" | "pending";
};

// ADR-015 — sidebar route inventory. Each entry becomes a page during
// Phase 1. The scaffold lands only HomePage; subsequent commits fill
// the rest in. Icons mirror the Iconize convention from ADR-014.
const ROUTES: RouteEntry[] = [
  { path: "/",          label: "dashboard", icon: LayoutDashboard, group: "primary",       impl: "scaffolded" },
  { path: "/chat",      label: "chat",      icon: MessageSquare,   group: "primary",       impl: "pending"    },
  { path: "/news",      label: "news",      icon: Newspaper,       group: "primary",       impl: "scaffolded" },
  { path: "/sessions",  label: "sessions",  icon: Terminal,        group: "observability", impl: "pending"    },
  { path: "/skills",    label: "skills",    icon: Sparkles,        group: "observability", impl: "scaffolded" },
  { path: "/reminders", label: "reminders", icon: Bell,            group: "observability", impl: "pending"    },
  { path: "/diary",     label: "diary",     icon: BookOpen,        group: "observability", impl: "pending"    },
  { path: "/vault",     label: "vault",     icon: Database,        group: "observability", impl: "pending"    },
  { path: "/cron",      label: "cron",      icon: Timer,           group: "observability", impl: "scaffolded" },
];

function PendingPage({ label, Icon }: { label: string; Icon: LucideIcon }) {
  return (
    <div className="p-10">
      <div className="flex items-center gap-3 text-[var(--color-nucleus-faint)]">
        <Icon size={20} strokeWidth={1.5} />
        <span className="text-base">
          [{label}] <span className="text-[var(--color-nucleus-accent)]">▸</span> not yet ported
        </span>
      </div>
      <div className="mt-2 text-sm text-[var(--color-nucleus-faint)] opacity-70">
        ADR-015 Phase 1 — lands in a subsequent commit on this branch.
      </div>
    </div>
  );
}

export default function App() {
  return (
    <div className="flex h-full">
      <Sidebar />
      <main className="flex flex-1 flex-col overflow-hidden">
        <TopBar />
        <div className="flex-1 overflow-auto">
          <Routes>
            <Route path="/" element={<HomePage />} />
            <Route path="/news" element={<NewsPage />} />
            <Route path="/cron" element={<CronPage />} />
            <Route path="/skills" element={<SkillsPage />} />
            {ROUTES.filter((r) => r.impl === "pending").map((r) => (
              <Route key={r.path} path={r.path} element={<PendingPage label={r.label} Icon={r.icon} />} />
            ))}
          </Routes>
        </div>
      </main>
    </div>
  );
}

function Sidebar() {
  const primary = ROUTES.filter((r) => r.group === "primary");
  const observability = ROUTES.filter((r) => r.group === "observability");

  return (
    <nav className="flex w-60 shrink-0 flex-col border-r border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <div className="border-b border-[var(--color-nucleus-border)] px-4 py-4">
        <div className="text-base tracking-wide">
          [<span className="text-[var(--color-nucleus-accent)]">nucleus</span>]
        </div>
        <div className="mt-0.5 text-xs text-[var(--color-nucleus-faint)]">
          operator dashboard
        </div>
      </div>

      <div className="flex-1 overflow-y-auto px-2 py-4">
        <SidebarSection label="surfaces" items={primary} />
        <div className="my-4 border-t border-[var(--color-nucleus-border)]" />
        <SidebarSection label="observability" items={observability} />
      </div>

      <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs text-[var(--color-nucleus-faint)]">
        <div className="flex items-center gap-2">
          <span className="inline-block h-1.5 w-1.5 rounded-full bg-[var(--color-status-ok)]" />
          <span>ADR-015 · scaffold</span>
        </div>
      </div>
    </nav>
  );
}

function SidebarSection({ label, items }: { label: string; items: RouteEntry[] }) {
  return (
    <div>
      <div className="mb-1.5 px-2 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
        {label}
      </div>
      <ul className="space-y-0.5">
        {items.map((r) => (
          <li key={r.path}>
            <NavLink
              to={r.path}
              end={r.path === "/"}
              className={({ isActive }) =>
                [
                  "group flex items-center gap-2.5 rounded px-2 py-1.5 text-sm transition-colors",
                  isActive
                    ? "bg-[color-mix(in_srgb,var(--color-nucleus-accent)_12%,transparent)] text-[var(--color-nucleus-accent)]"
                    : "text-[var(--color-nucleus-text)] hover:bg-[var(--color-nucleus-bg)] hover:text-[var(--color-nucleus-accent)]",
                  r.impl === "pending" ? "opacity-55" : "",
                ].join(" ")
              }
            >
              <r.icon size={15} strokeWidth={1.75} className="shrink-0" />
              <span>{r.label}</span>
              {r.impl === "pending" && (
                <span className="ml-auto text-[10px] text-[var(--color-nucleus-faint)] opacity-70">soon</span>
              )}
            </NavLink>
          </li>
        ))}
      </ul>
    </div>
  );
}

function TopBar() {
  const location = useLocation();
  const current = ROUTES.find((r) => (r.path === "/" ? location.pathname === "/" : location.pathname.startsWith(r.path)));
  const Icon = current?.icon ?? LayoutDashboard;

  return (
    <header className="flex shrink-0 items-center justify-between border-b border-[var(--color-nucleus-border)] px-5 py-3">
      <div className="flex items-center gap-2 text-sm">
        <span className="text-[var(--color-nucleus-faint)]">[</span>
        <span className="text-[var(--color-nucleus-accent)]">nucleus</span>
        <span className="text-[var(--color-nucleus-faint)]">]</span>
        <span className="text-[var(--color-nucleus-faint)]">/</span>
        <span className="flex items-center gap-1.5 text-[var(--color-nucleus-text)]">
          <Icon size={14} strokeWidth={1.75} />
          {current?.label ?? "dashboard"}
        </span>
      </div>

      <div className="flex items-center gap-4 text-xs text-[var(--color-nucleus-faint)]">
        <ConnectionPill />
      </div>
    </header>
  );
}

function ConnectionPill() {
  return (
    <div className="flex items-center gap-1.5">
      <Activity size={12} strokeWidth={2} className="text-[var(--color-status-ok)]" />
      <span>connected</span>
    </div>
  );
}
