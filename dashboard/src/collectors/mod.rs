//! Health collectors for the dashboard.
//!
//! Each collector implements `nucleus_core::health::HealthCheck`.

pub mod docker;
pub mod fetcher;
pub mod hermes;
pub mod tunnel;
pub mod self_check;
