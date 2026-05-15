//! Shared `claude` CLI types.
//!
//! Historically this module wrapped `claude -p` (headless mode) via a
//! `Runner` struct. That path is deprecated — `-p` is moving to API-only
//! billing, so every Nucleus bot/job now uses [`crate::claude_session`]
//! instead (long-lived interactive sessions in tmux). Only the shared
//! `PermissionMode` enum lives here now.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    Auto,
    DontAsk,
}

impl PermissionMode {
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::Auto => "auto",
            Self::DontAsk => "dontAsk",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypassPermissions" => Some(Self::BypassPermissions),
            "auto" => Some(Self::Auto),
            "dontAsk" => Some(Self::DontAsk),
            _ => None,
        }
    }
}
