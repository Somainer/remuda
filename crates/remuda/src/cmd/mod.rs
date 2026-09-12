//! Process subcommands mounted from `main`.

pub mod fleet;
pub mod hub_client;
pub mod instance;
pub mod mcp;
pub mod ssh;

#[cfg(test)]
pub(crate) mod test_hub;
