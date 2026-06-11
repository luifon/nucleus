// Agents API — the ADR-016 front door. Mirrors
// nucleus-dashboard/api/src/handlers/agents.rs.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, qs } from "./client";
import type { AgentView as AgentViewWire } from "./generated/AgentView";
import type { RunRow } from "./generated/RunRow";
import type { LogResponse as AgentLog } from "./generated/LogResponse";

export type { AgentClass } from "./generated/AgentClass";
export type { Launch } from "./generated/Launch";
export type { Capability } from "./generated/Capability";
export type { RunRow } from "./generated/RunRow";
export type { LogResponse as AgentLog } from "./generated/LogResponse";

/** UI-layer refinement: the wire shape (generated AgentView) carries
 *  `status: string`; this union narrows it to the values the handler
 *  actually emits. Not a Rust enum, so it stays hand-written here. */
export type AgentStatus =
  | "running"
  | "idle"
  | "errored"
  | "hosted"
  | "stopped"
  | "unknown";

/** Wire shape is generated; `status` narrowing is a UI-layer refinement. */
export type AgentView = Omit<AgentViewWire, "status"> & {
  status: AgentStatus;
};

export const listAgents = () => jsonGet<AgentView[]>("/agents/api/list");

export const listRuns = (agent: string) =>
  jsonGet<RunRow[]>(`/agents/api/runs${qs({ agent })}`);

export const getAgentLog = (agent: string) =>
  jsonGet<AgentLog>(`/agents/api/log${qs({ agent })}`);
