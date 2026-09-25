//! Operator-facing subcommands of the `nucleus` binary that live in core
//! because they operate on core's own data (the ADR-023 session index, the
//! ADR-021 session-messaging primitive, the ADR-035 vault index, the
//! ADR-033 task ledger, the ADR-036 intake store) rather than on a
//! service's state.

pub mod events;
pub mod intake;
pub mod session_search;
pub mod session_send;
pub mod usage;
pub mod vault_search;
pub mod tasks;
