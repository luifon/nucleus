// Barrel — re-exports the domain modules whose consumers import from
// "@/lib/api". gallery and documents are imported by path instead, so
// they're intentionally not re-exported here. Don't add aggregated
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
