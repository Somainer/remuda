//! Loopback Anthropic-Messages gateway double, for driving an api-routing
//! test from another process.
//!
//! Thin shim: the server, the scripts and the recorder live in
//! [`remuda_testing::fake_gateway`]. See `run_fake_gateway` for the
//! environment it reads and the readiness line it prints.

fn main() {
    if let Err(err) = remuda_testing::run_fake_gateway() {
        eprintln!("fake-gateway: {err}");
        std::process::exit(1);
    }
}
