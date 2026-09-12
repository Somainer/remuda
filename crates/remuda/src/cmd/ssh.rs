//! `remuda ssh` — composition-root hook for [`remuda_ssh::cli`].

#[derive(clap::Args)]
#[command(about = "SSH remote hosts: list, probe, bootstrap, and stdio node.")]
pub(crate) struct Args {
    #[command(flatten)]
    args: remuda_ssh::SshArgs,
}

impl super::registry::Entrypoint for Args {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        remuda_ssh::run_blocking(self.args).map(|()| 0)
    }
}
