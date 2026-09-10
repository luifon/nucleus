//! Operator-facing subcommands of the `nucleus` binary that live in core
//! because they operate on core's own data (the ADR-023 session index and the
//! ADR-021 session-messaging primitive) rather than on a service's state.

pub mod session_search;
pub mod session_send;
