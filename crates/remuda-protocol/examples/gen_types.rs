//! Generate or check the committed wire schema and TypeScript; `protocol.md` §12.

use remuda_protocol::schema::{schema_document, typescript};
use std::{error::Error, path::Path};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = match args.as_slice() {
        [] => false,
        [arg] if arg == "--check" => true,
        _ => {
            return Err(
                "usage: cargo run -p remuda-protocol --example gen_types -- [--check]".into(),
            );
        }
    };
    let schema = schema_document();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let outputs = [
        (
            "crates/remuda-protocol/schema/protocol.schema.json",
            serde_json::to_string_pretty(&schema)? + "\n",
        ),
        ("web/src/types/generated.ts", typescript(&schema)?),
    ];
    for (relative, content) in outputs {
        let path = root.join(relative);
        if check {
            if std::fs::read_to_string(&path)? != content {
                return Err(format!("{relative} is stale; run just gen-types").into());
            }
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)?;
        }
        println!("{} {relative}", if check { "Checked" } else { "Generated" });
    }
    Ok(())
}
