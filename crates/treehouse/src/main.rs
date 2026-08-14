//! treehouse: the CLI binary crate.
//!
//! Thin adapter over `treehouse-core`: parses clap args, invokes the pool, and
//! renders each command's structured result as human/json/toon. Business logic
//! lives in `treehouse-core`; this crate only owns the CLI surface.

use anyhow::Result;

fn main() -> Result<()> {
    // Placeholder entry point. The clap CLI tree is added by the CLI-surface bead.
    println!("treehouse {}", treehouse_core::VERSION);
    Ok(())
}
