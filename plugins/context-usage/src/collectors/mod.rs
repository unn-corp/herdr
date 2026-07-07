//! Per-CLI collectors that translate an agent's local telemetry into a
//! [`crate::cache::UsageRecord`].
//!
//! Phase 1 ships the Claude Code statusLine collector. Codex, Antigravity,
//! OpenCode, and Hermes adapters land in later phases behind the same cache
//! contract.

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod hermes;
pub mod opencode;
