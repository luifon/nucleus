import { Routes, Route, NavLink } from "react-router-dom";
import HomePage from "./pages/HomePage";

// ADR-015 — sidebar route inventory. Each entry becomes a page during
// Phase 1. The scaffold lands only HomePage; subsequent commits fill
// the rest in.
const ROUTES: { path: string; label: string; impl?: "scaffolded" | "pending" }[] = [
  { path: "/",          label: "dashboard", impl: "scaffolded" },
  { path: "/chat",      label: "chat",      impl: "pending" },
  { path: "/sessions",  label: "sessions",  impl: "pending" },
  { path: "/skills",    label: "skills",    impl: "pending" },
  { path: "/reminders", label: "reminders", impl: "pending" },
  { path: "/diary",     label: "diary",     impl: "pending" },
  { path: "/vault",     label: "vault",     impl: "pending" },
  { path: "/cron",      label: "cron",      impl: "pending" },
  { path: "/news",      label: "news",      impl: "pending" },
];

function PendingPage({ label }: { label: string }) {
  return (
    <div className="p-8">
      <div className="text-[var(--color-nucleus-faint)]">
        [{label}] <span className="text-[var(--color-nucleus-accent)]">▸</span> not yet ported (ADR-015 Phase 1)
      </div>
    </div>
  );
}

export default function App() {
  return (
    <div className="flex h-full">
      <Sidebar />
      <main className="flex-1 overflow-auto">
        <TopBar />
        <Routes>
          <Route path="/" element={<HomePage />} />
          {ROUTES.filter((r) => r.impl === "pending").map((r) => (
            <Route key={r.path} path={r.path} element={<PendingPage label={r.label} />} />
          ))}
        </Routes>
      </main>
    </div>
  );
}

function Sidebar() {
  return (
    <nav className="w-56 shrink-0 border-r border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3">
      <div className="mb-4 px-2 text-xs">
        [<span className="text-[var(--color-nucleus-accent)]">nucleus</span>]
      </div>
      <ul className="space-y-0.5">
        {ROUTES.map((r) => (
          <li key={r.path}>
            <NavLink
              to={r.path}
              end={r.path === "/"}
              className={({ isActive }) =>
                [
                  "block px-2 py-1 text-xs",
                  isActive
                    ? "text-[var(--color-nucleus-accent)]"
                    : "text-[var(--color-nucleus-text)] hover:text-[var(--color-nucleus-accent)]",
                  r.impl === "pending" ? "opacity-60" : "",
                ].join(" ")
              }
            >
              <span className="opacity-50">▸ </span>
              {r.label}
            </NavLink>
          </li>
        ))}
      </ul>
    </nav>
  );
}

function TopBar() {
  return (
    <header className="flex items-center justify-between border-b border-[var(--color-nucleus-border)] px-4 py-2 text-xs">
      <div className="text-[var(--color-nucleus-faint)]">
        [<span className="text-[var(--color-nucleus-accent)]">nucleus</span>] / dashboard
      </div>
      <div className="text-[var(--color-nucleus-faint)]">
        <span className="text-[var(--color-nucleus-accent)]">▸</span> scaffold
      </div>
    </header>
  );
}
