//! treehouse-core: pure library port of Treehouse (Go v2.1.1 behavioral reference).
//!
//! This crate contains the pool/state/lease/owner/process/git/hooks/config
//! logic with **no** CLI concerns (clap, anyhow, owo-colors live only in the
//! `treehouse` binary crate). Each command produces a structured [`CommandResult`]
//! that the CLI formats as human/json/toon.

/// Library version, kept in sync with the workspace `version` field.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod config;
pub mod destroy;
pub mod git;
pub mod hooks;
pub mod lease;
pub mod lock;
pub mod pool;
pub mod process;
pub mod prune;
pub mod reservation;
pub mod result;
pub mod state;
pub mod state_file;
pub mod worktree;

#[cfg(all(test, feature = "toon"))]
mod toon_smoke_tests {
    use serde_json::json;

    #[test]
    fn toon_encoder_smoke() {
        let value = json!({
            "name": "Alice",
            "age": 30,
            "tags": ["rust", "toon"]
        });
        let encoded = toon::encode(value.clone(), None);
        assert!(encoded.contains("name: Alice"), "got: {encoded}");
        assert!(encoded.contains("tags[2]"), "got: {encoded}");
        // Round-trips back to a JsonValue.
        let decoded = toon::try_decode(&encoded, None).unwrap();
        let _ = decoded;
    }
}
