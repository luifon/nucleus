import { jsonGet } from "./client";

export type Health = {
  status: string;
  service: string;
  version: string;
};

export const getHealth = () => jsonGet<Health>("/api/health");
