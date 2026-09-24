// Usage API — token and estimated-cost accounting (ADR-034).
// Mirrors `nucleus-dashboard/api/src/handlers/usage.rs`.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { UsageStatus } from "./generated/UsageStatus";
import type { UsageSummary } from "./generated/UsageSummary";
import type { UsageProjectRow } from "./generated/UsageProjectRow";
import type { UsageNucleusView } from "./generated/UsageNucleusView";
import type { UsageLimits } from "./generated/UsageLimits";
import type { UsageSessionRow } from "./generated/UsageSessionRow";
import type { RefreshStarted } from "./generated/RefreshStarted";
import type { VendorFilter } from "./generated/VendorFilter";

export type { VendorFilter } from "./generated/VendorFilter";

export type { UsageStatus } from "./generated/UsageStatus";
export type { UsageSummary } from "./generated/UsageSummary";
export type { UsageTotals } from "./generated/UsageTotals";
export type { UsageCompare } from "./generated/UsageCompare";
export type { UsageSeriesPoint } from "./generated/UsageSeriesPoint";
export type { UsageModelRow } from "./generated/UsageModelRow";
export type { UsageHeatCell } from "./generated/UsageHeatCell";
export type { UsageProjectRow } from "./generated/UsageProjectRow";
export type { UsageNucleusView } from "./generated/UsageNucleusView";
export type { UsageAgentRow } from "./generated/UsageAgentRow";
export type { UsageReminderRow } from "./generated/UsageReminderRow";
export type { UsageLimits } from "./generated/UsageLimits";
export type { UsageLimitEvent } from "./generated/UsageLimitEvent";
export type { UsageRatePoint } from "./generated/UsageRatePoint";
export type { UsageRateReading } from "./generated/UsageRateReading";
export type { UsageSessionRow } from "./generated/UsageSessionRow";
export type { UsagePrice } from "./generated/UsagePrice";

export const getUsageStatus = (signal?: AbortSignal) => jsonGet<UsageStatus>("/usage/api/status", signal);

export const startUsageRefresh = () => jsonPost<RefreshStarted, Record<string, never>>("/usage/api/refresh", {});

export const getUsageSummary = (days: number, vendor: VendorFilter, signal?: AbortSignal) =>
  jsonGet<UsageSummary>(`/usage/api/summary${qs({ days, vendor })}`, signal);

export const getUsageProjects = (days: number, vendor: VendorFilter, signal?: AbortSignal) =>
  jsonGet<UsageProjectRow[]>(`/usage/api/projects${qs({ days, vendor })}`, signal);

export const getUsageNucleus = (days: number, vendor: VendorFilter, signal?: AbortSignal) =>
  jsonGet<UsageNucleusView>(`/usage/api/nucleus${qs({ days, vendor })}`, signal);

export const getUsageLimits = (days: number, vendor: VendorFilter, signal?: AbortSignal) =>
  jsonGet<UsageLimits>(`/usage/api/limits${qs({ days, vendor })}`, signal);

export const getUsageSessions = (days: number, limit: number, vendor: VendorFilter, signal?: AbortSignal) =>
  jsonGet<UsageSessionRow[]>(`/usage/api/sessions${qs({ days, limit, vendor })}`, signal);
