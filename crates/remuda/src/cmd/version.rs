//! Build identity is available without configuration or tracing initialization.

#[derive(clap::Args)]
#[command(about = "Print build identity without loading configuration or starting services.")]
pub(crate) struct Args {
    /// Emit exactly one JSON object on stdout.
    #[arg(long)]
    pub(crate) json: bool,
}

impl super::registry::Entrypoint for Args {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        crate::build_info::write(self.json, &mut std::io::stdout().lock()).map(|()| 0)
    }

    fn tracing(&self) -> bool {
        false
    }
}
