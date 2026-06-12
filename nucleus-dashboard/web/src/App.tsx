import { useState, useEffect } from "react";
import { Routes, Route, NavLink, useLocation } from "react-router-dom";
import {
  LayoutDashboard,
  MessageSquare,
  Boxes,
  Sparkles,
  Bell,
  BookOpen,
  Database,
  Newspaper,
  Image as ImageIcon,
  FolderLock,
  Activity,
  Menu,
  X,
  type LucideIcon,
} from "lucide-react";
import HomePage from "./pages/HomePage";
import NewsPage from "./pages/NewsPage";
import SkillsPage from "./pages/SkillsPage";
import DiaryPage from "./pages/DiaryPage";
import RemindersPage from "./pages/RemindersPage";
import AgentsPage from "./pages/AgentsPage";
import VaultPage from "./pages/VaultPage";
import ChatPage from "./pages/ChatPage";
import GalleryPage from "./pages/GalleryPage";
import DocumentsPage from "./pages/DocumentsPage";
import { ErrorBoundary } from "./components/ErrorBoundary";

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
  { path: "/chat",      label: "chat",      icon: MessageSquare,   group: "primary",       impl: "scaffolded" },
  { path: "/gallery",   label: "gallery",   icon: ImageIcon,       group: "primary",       impl: "scaffolded" },
  { path: "/documents", label: "documents", icon: FolderLock,      group: "primary",       impl: "scaffolded" },
  { path: "/news",      label: "news",      icon: Newspaper,       group: "primary",       impl: "scaffolded" },
  { path: "/agents",    label: "agents",    icon: Boxes,           group: "observability", impl: "scaffolded" },
  { path: "/skills",    label: "skills",    icon: Sparkles,        group: "observability", impl: "scaffolded" },
  { path: "/reminders", label: "reminders", icon: Bell,            group: "observability", impl: "scaffolded" },
  { path: "/diary",     label: "diary",     icon: BookOpen,        group: "observability", impl: "scaffolded" },
  { path: "/vault",     label: "vault",     icon: Database,        group: "observability", impl: "scaffolded" },
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
  const [navOpen, setNavOpen] = useState(false);
  const location = useLocation();

  // Close the mobile drawer whenever the route changes.
  useEffect(() => {
    setNavOpen(false);
  }, [location.pathname]);

  return (
    <div className="flex h-full">
      {/* Backdrop — mobile only, dismisses the drawer on tap. */}
      {navOpen && (
        <div
          className="fixed inset-0 z-40 bg-black/60 md:hidden"
          onClick={() => setNavOpen(false)}
          aria-hidden
        />
      )}
      <Sidebar open={navOpen} onClose={() => setNavOpen(false)} />
      <main className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <TopBar onMenuToggle={() => setNavOpen((v) => !v)} navOpen={navOpen} />
        <div className="flex-1 overflow-auto">
          {/* Keyed by pathname so navigating away from a crashed page
              auto-resets the boundary; sidebar/topbar stay alive. */}
          <ErrorBoundary key={location.pathname}>
            <Routes>
              <Route path="/" element={<HomePage />} />
              <Route path="/news" element={<NewsPage />} />
              <Route path="/skills" element={<SkillsPage />} />
              <Route path="/diary" element={<DiaryPage />} />
              <Route path="/reminders" element={<RemindersPage />} />
              <Route path="/agents" element={<AgentsPage />} />
              <Route path="/vault" element={<VaultPage />} />
              <Route path="/chat" element={<ChatPage />} />
              <Route path="/gallery" element={<GalleryPage />} />
              <Route path="/documents" element={<DocumentsPage />} />
              {ROUTES.filter((r) => r.impl === "pending").map((r) => (
                <Route key={r.path} path={r.path} element={<PendingPage label={r.label} Icon={r.icon} />} />
              ))}
            </Routes>
          </ErrorBoundary>
        </div>
      </main>
    </div>
  );
}

function Sidebar({ open, onClose }: { open: boolean; onClose: () => void }) {
  const primary = ROUTES.filter((r) => r.group === "primary");
  const observability = ROUTES.filter((r) => r.group === "observability");

  return (
    <nav
      className={[
        "flex w-60 shrink-0 flex-col border-r border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]",
        // Mobile: off-canvas drawer slid in from the left. Desktop (md+):
        // a static column in normal flow, always visible.
        "fixed inset-y-0 left-0 z-50 transition-transform duration-200 ease-out md:static md:z-auto md:translate-x-0",
        open ? "translate-x-0" : "-translate-x-full",
      ].join(" ")}
    >
      <div className="flex items-center justify-between border-b border-[var(--color-nucleus-border)] px-4 py-4">
        <div>
          <div className="text-base tracking-wide">
            [<span className="text-[var(--color-nucleus-accent)]">nucleus</span>]
          </div>
          <div className="mt-0.5 text-xs text-[var(--color-nucleus-faint)]">
            operator dashboard
          </div>
        </div>
        <button
          onClick={onClose}
          aria-label="close menu"
          className="md:hidden text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <X size={18} strokeWidth={1.75} />
        </button>
      </div>

      <div className="flex-1 overflow-y-auto px-2 py-4">
        <SidebarSection label="surfaces" items={primary} onNavigate={onClose} />
        <div className="my-4 border-t border-[var(--color-nucleus-border)]" />
        <SidebarSection label="observability" items={observability} onNavigate={onClose} />
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

function SidebarSection({
  label,
  items,
  onNavigate,
}: {
  label: string;
  items: RouteEntry[];
  onNavigate: () => void;
}) {
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
              onClick={onNavigate}
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

function TopBar({ onMenuToggle, navOpen }: { onMenuToggle: () => void; navOpen: boolean }) {
  const location = useLocation();
  const current = ROUTES.find((r) => (r.path === "/" ? location.pathname === "/" : location.pathname.startsWith(r.path)));
  const Icon = current?.icon ?? LayoutDashboard;

  return (
    <header className="flex shrink-0 items-center justify-between border-b border-[var(--color-nucleus-border)] px-4 py-3 md:px-5">
      <div className="flex min-w-0 items-center gap-2 text-sm">
        <button
          onClick={onMenuToggle}
          aria-label="toggle menu"
          aria-expanded={navOpen}
          className="md:hidden text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <Menu size={18} strokeWidth={1.75} />
        </button>
        {/* [nucleus] / prefix — hidden on the narrowest screens to leave room. */}
        <span className="hidden items-center gap-2 sm:flex">
          <span className="text-[var(--color-nucleus-faint)]">[</span>
          <span className="text-[var(--color-nucleus-accent)]">nucleus</span>
          <span className="text-[var(--color-nucleus-faint)]">]</span>
          <span className="text-[var(--color-nucleus-faint)]">/</span>
        </span>
        <span className="flex min-w-0 items-center gap-1.5 text-[var(--color-nucleus-text)]">
          <Icon size={14} strokeWidth={1.75} className="shrink-0" />
          <span className="truncate">{current?.label ?? "dashboard"}</span>
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
