//! Internal helper invoked by shell completion scripts.
//!
//! Prints every discovered agent name on its own line, sorted. Errors
//! are swallowed silently — completion must never break a user's shell,
//! and an empty list is a valid fallback.

use anyhow::Result;

use crate::discovery;

pub fn run() -> Result<()> {
    let mut names: Vec<String> = match discovery::discover_all() {
        Ok(agents) => agents.into_iter().map(|a| a.manifest.agent.name).collect(),
        Err(_) => return Ok(()),
    };
    names.sort();
    names.dedup();
    for n in names {
        println!("{n}");
    }
    Ok(())
}
