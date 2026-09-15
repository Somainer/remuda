//! Command registration. Feature modules own arguments, help and execution.

pub mod agents;
pub mod dev;
pub mod dispatcher;
pub mod doctor;
pub mod fleet;
pub mod hook;
pub mod hub;
pub mod hub_client;
mod hub_maintenance;
pub mod instance;
mod instance_interaction;
pub mod journal_diff;
pub mod mcp;
pub mod merge;
pub mod node;
mod own;
mod profile;
mod project;
pub(crate) mod registry;
pub mod ssh;
pub mod table;
mod task;
pub mod version;
pub mod worktree;

macro_rules! commands {
    ($($variant:ident($module:ident::$args:ident)),* $(,)?) => {
        #[derive(clap::Subcommand)]
        pub(crate) enum Command { $($variant($module::$args)),* }
        impl Command {
            pub fn run(self, context: registry::Context) -> anyhow::Result<i32> {
                use registry::Entrypoint;
                match self { $(Self::$variant(args) => args.enter(context)),* }
            }
            pub fn tracing(&self) -> bool {
                use registry::Entrypoint;
                match self { $(Self::$variant(args) => args.tracing()),* }
            }
        }
    };
}

// Declare a module above, then register its parser and dispatch once below.
commands! {
    Hub(hub::Args),
    Node(node::Args),
    Dev(dev::Args),
    Dispatcher(dispatcher::Args),
    Version(version::Args),
    Ssh(ssh::Args),
    Instance(instance::Args),
    Journal(journal_diff::Args),
    Project(project::ProjectArgs),
    Task(task::TaskArgs),
    Own(own::OwnArgs),
    Profile(profile::ProfileArgs),
    Fleet(fleet::Args),
    Worktree(worktree::Args),
    Merge(merge::MergeArgs),
    Doctor(doctor::CommandArgs),
    Agents(agents::CommandArgs),
    Mcp(mcp::Args),
    Hook(hook::CommandArgs),
}

#[cfg(test)]
pub(crate) mod test_hub;
