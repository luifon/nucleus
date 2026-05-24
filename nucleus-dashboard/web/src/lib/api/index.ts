// Barrel — re-exports everything so callers can keep
// `import { foo } from "@/lib/api"` regardless of how the per-domain
// files are split internally.
//
// One re-export line per domain module. Don't add aggregated helpers
// here — those belong in the relevant domain file.

export * from "./client";
export * from "./health";
export * from "./news";
export * from "./cron";
export * from "./skills";
export * from "./diary";
export * from "./reminders";
export * from "./agents";
export * from "./vault";
export * from "./chat";
export * from "./dashboard";
