//! Reminders library — the data layer behind the `reminders` CLI
//! (`src/main.rs`) and the nucleus-dashboard reminders surface.
//!
//! Exposing `store` as a lib (rather than a binary-internal `mod
//! store;`) lets the dashboard share the exact schema, helpers, and
//! invariants the CLI uses — no SQL-shaped drift. The CLI imports
//! `reminders::store` instead of its own `mod store`.

pub mod store;
