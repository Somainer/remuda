//! Claude `apiKeyHelper`: print one secret from the Node token-broker UDS.
//!
//! Production launches use the script [`remuda_driver::render_api_key_helper_script`]
//! writes next to `--settings`. This binary is the same JSON-line protocol.
//!
//! Environment:
//! - `REMUDA_SECRET_SOCK` — absolute path of the broker socket
//! - `REMUDA_INSTANCE_ID` — allowlisted instance id
//! - `REMUDA_INSTANCE_TOKEN` — per-instance bearer
//! - `REMUDA_SECRET_REF` — e.g. `store:anthropic`
//!
//! stdout is only the secret plus a trailing newline. Errors go to stderr.

/// Claude `apiKeyHelper` entry: print one secret or exit 1.
#[cfg(unix)]
#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

/// Fetch one allowlisted secret from the token broker and print it.
#[cfg(unix)]
async fn run() -> Result<(), remuda_driver::DriverError> {
    use remuda_driver::{SecretRef, request_secret};
    use std::path::PathBuf;

    let sock = PathBuf::from(std::env::var("REMUDA_SECRET_SOCK").map_err(|_| {
        remuda_driver::DriverError::CredentialUnavailable("REMUDA_SECRET_SOCK is unset".into())
    })?);
    let instance = std::env::var("REMUDA_INSTANCE_ID").map_err(|_| {
        remuda_driver::DriverError::CredentialUnavailable("REMUDA_INSTANCE_ID is unset".into())
    })?;
    let token = std::env::var("REMUDA_INSTANCE_TOKEN").map_err(|_| {
        remuda_driver::DriverError::CredentialUnavailable("REMUDA_INSTANCE_TOKEN is unset".into())
    })?;
    let secret_ref = SecretRef::parse(std::env::var("REMUDA_SECRET_REF").map_err(|_| {
        remuda_driver::DriverError::CredentialUnavailable("REMUDA_SECRET_REF is unset".into())
    })?)?;
    let secret = request_secret(&sock, &instance, &token, &secret_ref).await?;
    println!("{}", secret.expose_str()?);
    Ok(())
}

/// Claude `apiKeyHelper` is Unix-only.
#[cfg(not(unix))]
fn main() {
    eprintln!("api-key-helper requires a Unix domain socket");
    std::process::exit(1);
}
