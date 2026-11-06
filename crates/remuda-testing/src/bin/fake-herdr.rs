//! Herdr JSON-RPC Unix-socket test double.

use remuda_testing::run_fake_herdr;

fn main() {
    remuda_testing::embed_build_stamp();
    match run_fake_herdr() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("fake-herdr: {err}");
            std::process::exit(1);
        }
    }
}
