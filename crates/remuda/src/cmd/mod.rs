//! Process subcommands mounted from `main`.

pub mod agents;
pub mod doctor;
pub mod fleet;
pub mod hub_client;
pub mod instance;
mod instance_interaction;
pub mod mcp;
pub mod merge;
pub mod ssh;
pub mod table;
pub mod worktree;

#[cfg(test)]
pub(crate) mod test_hub;
