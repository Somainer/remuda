//! Build identity shared by the human and JSON version command.

use serde::Serialize;
use std::io::Write;

#[derive(Serialize)]
struct BuildInfo {
    version: &'static str,
    git_sha: &'static str,
    build_date: &'static str,
    rustc: &'static str,
    target: &'static str,
    wire_major: u16,
    // Retained CLI compatibility indicator; not a database migration authority.
    schema_major: u16,
}

impl BuildInfo {
    fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            git_sha: env!("REMUDA_GIT_SHA"),
            build_date: env!("REMUDA_BUILD_DATE"),
            rustc: env!("REMUDA_RUSTC_VERSION"),
            target: env!("REMUDA_TARGET"),
            wire_major: remuda_protocol::PROTOCOL_VERSION.major,
            schema_major: 0,
        }
    }
}

pub(crate) fn write(json: bool, output: &mut impl Write) -> anyhow::Result<()> {
    let info = BuildInfo::current();
    if json {
        serde_json::to_writer(&mut *output, &info)?;
        writeln!(output)?;
    } else {
        writeln!(
            output,
            "remuda {}\ncommit={}\nbuild_date={}\nrustc={}\ntarget={}\nwire={}\nschema={}",
            info.version,
            info.git_sha,
            info.build_date,
            info.rustc,
            info.target,
            info.wire_major,
            info.schema_major
        )?;
    }
    Ok(())
}
