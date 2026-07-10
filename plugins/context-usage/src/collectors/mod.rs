//! Per-CLI collectors that translate an agent's local telemetry into a
//! [`crate::cache::UsageRecord`].
//!
//! Claude and Antigravity are push collectors (statusLine hooks). Codex,
//! OpenCode, Hermes, and Grok are pull collectors driven by `poll`.

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod grok;
pub mod hermes;
pub mod opencode;
