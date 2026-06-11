// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet } from "./client";
import type { HealthOverview } from "./generated/HealthOverview";
import type { Glances } from "./generated/Glances";
import type { DockerResp } from "./generated/DockerResp";
import type { TunnelResp } from "./generated/TunnelResp";

export type { HealthCheck } from "./generated/HealthCheck";
export type { HealthOverview } from "./generated/HealthOverview";
export type { NextFireGlance } from "./generated/NextFireGlance";
export type { VaultGlance } from "./generated/VaultGlance";
export type { DiaryGlance } from "./generated/DiaryGlance";
export type { NewsGlance } from "./generated/NewsGlance";
export type { ChatGlance } from "./generated/ChatGlance";
export type { Glances } from "./generated/Glances";
export type { DockerContainer } from "./generated/DockerContainer";
export type { DockerResp } from "./generated/DockerResp";
export type { TunnelResp } from "./generated/TunnelResp";

export const getDashboardHealth = () => jsonGet<HealthOverview>("/api/dashboard/health");
export const getDashboardGlances = () => jsonGet<Glances>("/api/dashboard/glances");
export const getDashboardDocker = () => jsonGet<DockerResp>("/api/dashboard/docker");
export const getDashboardTunnel = () => jsonGet<TunnelResp>("/api/dashboard/tunnel");
