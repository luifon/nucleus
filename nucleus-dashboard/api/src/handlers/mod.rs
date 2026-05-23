// Handlers will be added surface-by-surface during Phase 1:
//   - sessions   — tmux inspector
//   - skills     — operator + developer skills
//   - reminders  — admin (wraps the reminders CLI)
//   - diary      — per-agent diary entries
//   - vault      — brain-dump write feed
//   - news       — public read API + admin
//   - chat       — WS + session lifecycle (lifted from chat/ crate)
//   - cron       — launchd + reminders aggregation
//
// Each becomes its own module here. The scaffold lands empty by design
// (ADR-015 Phase 1 step 1 — the surfaces follow in subsequent commits).
