//! Generate or check the machine half of the agent-parity ledger (D-060 §12).
//!
//! ```text
//! cargo run -p remuda --example gen_parity            # write the file
//! cargo run -p remuda --example gen_parity -- --check # fail if stale or drifted
//! ```

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = match args.as_slice() {
        [] => false,
        [arg] if arg == "--check" => true,
        _ => {
            return Err("usage: cargo run -p remuda --example gen_parity -- [--check]".into());
        }
    };
    let model = remuda::parity::machine_model()?;
    let rendered = remuda::parity::render(&model);
    let path = remuda::parity::repo_root().join(remuda::parity::GENERATED_FILE);
    if check {
        let committed = std::fs::read_to_string(&path)?;
        remuda::parity::check_generated(&model, &committed)?;
        let human =
            std::fs::read_to_string(remuda::parity::repo_root().join(remuda::parity::HUMAN_FILE))?;
        let drift = remuda::parity::drift(&model, &human);
        if !drift.is_empty() {
            for failure in &drift {
                eprintln!("{failure}");
            }
            return Err(format!("{} ledger drift failure(s)", drift.len()).into());
        }
        println!("Checked {}", remuda::parity::GENERATED_FILE);
    } else {
        std::fs::write(&path, rendered)?;
        println!("Generated {}", remuda::parity::GENERATED_FILE);
    }
    Ok(())
}
