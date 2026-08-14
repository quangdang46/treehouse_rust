//! treehouse-core: pure library port of Treehouse (Go v2.1.1 behavioral reference).
//!
//! This crate contains the pool/state/lease/owner/process/git/hooks/config
//! logic with **no** CLI concerns (clap, anyhow, owo-colors live only in the
//! `treehouse` binary crate). Each command produces a structured [`CommandResult`]
//! that the CLI formats as human/json/toon.

/// Library version, kept in sync with the workspace `version` field.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod git;
pub mod lease;
pub mod lock;
pub mod process;
pub mod reservation;
pub mod state;
pub mod state_file;
pub mod worktree;
