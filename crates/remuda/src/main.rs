//! Remuda binary: the composition root lives in the library
//! (`crates/remuda/src/lib.rs`) so the clap tree can be introspected.

fn main() -> anyhow::Result<()> {
    remuda::run()
}
