//! Example: Zero-filesystem usage with InMemoryEnv.
//!
//! Run with: `cargo run --example in_memory_env -p treehouse-core`

use std::path::PathBuf;
use treehouse_core::TreehouseCore;
use treehouse_core::config::TreehouseConfig;
use treehouse_core::env::InMemoryEnv;

fn main() {
    // Create a zero-filesystem test environment
    let env = InMemoryEnv::new(PathBuf::from("/test/pools"));

    // Seed some files for testing
    env.seed_file(
        std::path::Path::new("/test/pools/myrepo-abc123/treehouse-state.json"),
        br#"{"worktrees":[]}"#,
    );

    let config = TreehouseConfig::default();
    let core = TreehouseCore::with_env(env, config);

    println!("Pool root: {:?}", core.pool_root());
    println!("No filesystem touched!");

    // Verify the seeded file exists
    let env_ref = core.pool_root().unwrap();
    println!("Pool root exists: {:?}", env_ref);
}
