//! VAPID key generation and 0600 persistence (`vapid.json`).

use crate::Error;
use p256::SecretKey;
use p256::elliptic_curve::rand_core::OsRng;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use web_push::VapidSignatureBuilder;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KeysFile {
    pub(crate) private: String,
    pub(crate) public: String,
}

/// Load `data_dir/vapid.json` or generate a new P-256 keypair.
pub(crate) fn load_or_create(data_dir: &Path) -> Result<(KeysFile, PathBuf), Error> {
    fs::create_dir_all(data_dir)?;
    let path = data_dir.join("vapid.json");
    match fs::read(&path) {
        Ok(bytes) => {
            let keys: KeysFile = serde_json::from_slice(&bytes)?;
            if keys.private.is_empty() || keys.public.is_empty() {
                return Err(Error::Invalid("invalid VAPID key file".into()));
            }
            Ok((keys, path))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let keys = generate()?;
            write_0600(&path, &keys)?;
            Ok((keys, path))
        }
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn generate() -> Result<KeysFile, Error> {
    let secret = SecretKey::random(&mut OsRng);
    let private = b64(&secret.to_bytes());
    let builder = VapidSignatureBuilder::from_base64_no_sub(&private).map_err(Error::from)?;
    let public = b64(&builder.get_public_key());
    Ok(KeysFile { private, public })
}

fn write_0600(path: &Path, keys: &KeysFile) -> Result<(), Error> {
    let encoded = serde_json::to_vec_pretty(keys)?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(&encoded)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn b64_decode(text: &str) -> Result<Vec<u8>, Error> {
    use base64::Engine;
    let trimmed = text.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(text))
        .map_err(|_| Error::InvalidSubscription)
}
