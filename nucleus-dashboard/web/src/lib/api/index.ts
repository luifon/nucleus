// Barrel — re-exports the domain modules whose consumers import from
// "@/lib/api". documents is imported by path instead, so it's
// intentionally not re-exported here. Don't add aggregated
// helpers here — those belong in the relevant domain file.

export * from "./client";
export * from "./health";
export * from "./news";
export * from "./skills";
export * from "./diary";
export * from "./reminders";
export * from "./agents";
export * from "./vault";
export * from "./chat";
export * from "./dashboard";
export * from "./tasks";
export * from "./intake";
