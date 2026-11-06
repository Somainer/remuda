//! Stream-json stand-in for `claude -p`.

use remuda_testing::run_fake_claude;

fn main() {
    remuda_testing::embed_build_stamp();
    match run_fake_claude() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("fake-claude: {err}");
            std::process::exit(1);
        }
    }
}
