//! Fixture CLI context.

use std::path::PathBuf;

#[derive(clap::Args, Default)]
pub(crate) struct Context {
    /// Override the data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
}
