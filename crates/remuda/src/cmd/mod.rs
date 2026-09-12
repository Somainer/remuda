//! Command registration. Feature modules own arguments, help and execution.

pub mod hub_client;
mod instance_interaction;
pub(crate) mod registry;
pub mod table;

macro_rules! commands {
    ($($variant:ident($module:ident::$args:ident)),* $(,)?) => {
        $(pub mod $module;)*
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

// Adding a command requires its own module and one registration line.
commands! {
    Hub(hub::Args),
    Node(node::Args),
    Dev(dev::Args),
    Dispatcher(dispatcher::Args),
    Version(version::Args),
    Ssh(ssh::Args),
    Instance(instance::Args),
    Fleet(fleet::Args),
    Worktree(worktree::Args),
    Merge(merge::MergeArgs),
    Doctor(doctor::CommandArgs),
    Agents(agents::CommandArgs),
    Mcp(mcp::Args),
}

#[cfg(test)]
pub(crate) mod test_hub;
