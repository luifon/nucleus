// Agents API — the ADR-016 front door. Mirrors
// nucleus-dashboard/api/src/handlers/agents.rs.

import { jsonGet, qs } from "./client";

export type AgentClass =
  | "conversational"
  | "scheduled"
  | "maintenance"
  | "infra"
  | "ephemeral";

export type Launch = "launchd-daemon" | "launchd-cron" | "in-process" | "on-demand";

export type Capability = "rotates" | "skill_review";

export type AgentStatus =
  | "running"
  | "idle"
  | "errored"
  | "hosted"
  | "stopped"
  | "unknown";

export type AgentView = {
  name: string;
  class: AgentClass;
  launch: Launch;
  runtime: string | null;
  schedule: string | null;
  diary_key: string | null;
  persona_venue: string | null;
  persona_display_name: string | null;
  capabilities: Capability[];
  tmux_session: string | null;
  launchd_label: string | null;

  status: AgentStatus;
  pid: number | null;
  last_exit: number | null;
  live_windows: number;
  last_activity_unix: number | null;
  last_run_started: string | null;
  run_count: number;
  attach_cmd: string | null;
};

export type RunRow = {
  run_id: string;
  agent: string;
  session_id: string;
  transcript_path: string;
  tmux_target: string;
  started_at: string;
  ended_at: string | null;
  ok: boolean | null;
};

export type AgentLog = {
  agent: string;
  path: string;
  tail: string;
};

export const listAgents = () => jsonGet<AgentView[]>("/agents/api/list");

export const listRuns = (agent: string) =>
  jsonGet<RunRow[]>(`/agents/api/runs${qs({ agent })}`);

export const getAgentLog = (agent: string) =>
  jsonGet<AgentLog>(`/agents/api/log${qs({ agent })}`);
