import { jsonGet } from "./client";

export type HealthCheck = {
  name: string;
  ok: boolean;
  detail: string;
};

export type HealthOverview = {
  checks: HealthCheck[];
  ok_count: number;
  total: number;
};

export type NextFireGlance = {
  id: number;
  title_or_body: string;
  next_fire_at: string;
  channels: string | null;
};

export type VaultGlance = {
  relpath: string;
  bucket: string;
  mtime_unix: number;
};

export type DiaryGlance = {
  agent: string;
  date: string;
  first_section: string | null;
};

export type NewsGlance = {
  title: string;
  source_name: string;
  url: string;
  notable_score: number | null;
};

export type ChatGlance = {
  id: string;
  title: string | null;
  last_active: string;
};

export type Glances = {
  next_fire: NextFireGlance | null;
  latest_vault: VaultGlance | null;
  latest_diary: DiaryGlance | null;
  top_news: NewsGlance | null;
  latest_chat: ChatGlance | null;
};

export type DockerContainer = {
  id: string;
  names: string[];
  image: string;
  state: string;
  status: string;
};

export type DockerResp = {
  available: boolean;
  error: string | null;
  containers: DockerContainer[];
};

export type TunnelResp = {
  configured: boolean;
  url: string | null;
  ok: boolean;
  status_code: number | null;
  elapsed_ms: number | null;
  error: string | null;
};

export const getDashboardHealth = () => jsonGet<HealthOverview>("/api/dashboard/health");
export const getDashboardGlances = () => jsonGet<Glances>("/api/dashboard/glances");
export const getDashboardDocker = () => jsonGet<DockerResp>("/api/dashboard/docker");
export const getDashboardTunnel = () => jsonGet<TunnelResp>("/api/dashboard/tunnel");
