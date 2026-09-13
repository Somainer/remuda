//! Passkey (WebAuthn) ceremony tests against a real Hub (D-029).
//!
//! webauthn-rs-core ships no software authenticator, so this module contains
//! a minimal ES256/P-256 one: it builds the attestation object and assertion
//! bytes a browser/platform authenticator would produce, which is exactly
//! what lets us assert the security-relevant negative cases (wrong origin,
//! replay, deleted credential, counter rollback) end to end.

use anyhow::{Context, Result};
use openssl::bn::{BigNum, BigNumContext};
use openssl::ec::{EcGroup, EcKey};
use openssl::ecdsa::EcdsaSig;
use openssl::nid::Nid;
use remuda_hub::{HubConfig, spawn};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

const ORIGIN_HOST: &str = "127.0.0.1";

// ---------------------------------------------------------------------------
// HTTP plumbing (kept local and dependency-free, mirroring tests/hub.rs)
// ---------------------------------------------------------------------------

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), rest.to_string()))
}

fn set_cookie_token(head: &str) -> Option<String> {
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            let pair = value.split(';').next()?.trim();
            if pair.starts_with("remuda_device=") {
                return Some(pair.to_string());
            }
        }
    }
    None
}

async fn boot() -> Result<(remuda_hub::RunningHub, String, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let config = HubConfig::for_test(dir.path().join("data"));
    let hub = spawn(config).await?;
    let bootstrap = hub.bootstrap_token.clone();
    Ok((hub, bootstrap, dir))
}

async fn bootstrap_login(addr: std::net::SocketAddr, token: &str, origin: &str) -> Result<String> {
    let body =
        serde_json::json!({ "bootstrapToken": token, "deviceName": "test-device" }).to_string();
    let (status, head, _) = http(
        addr,
        "POST",
        "/v1/login",
        &[("Origin", origin)],
        Some(&body),
    )
    .await?;
    assert_eq!(status, 200);
    set_cookie_token(&head).context("login did not set a device cookie")
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(text: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text)?)
}

// ---------------------------------------------------------------------------
// Software ES256 authenticator
// ---------------------------------------------------------------------------

struct SoftAuthenticator {
    key: EcKey<openssl::pkey::Private>,
    credential_id: Vec<u8>,
    user_id: Vec<u8>,
    rp_id_hash: [u8; 32],
    counter: u32,
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn cbor_int(value: i128) -> serde_cbor_2::Value {
    serde_cbor_2::Value::Integer(value)
}

fn cbor_bytes(value: Vec<u8>) -> serde_cbor_2::Value {
    serde_cbor_2::Value::Bytes(value)
}

fn cbor_text(value: &str) -> serde_cbor_2::Value {
    serde_cbor_2::Value::Text(value.to_string())
}

impl SoftAuthenticator {
    fn new(rp_id: &str, user_id: Vec<u8>) -> Result<Self> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        let key = EcKey::generate(&group)?;
        // Deterministic-looking random credential id.
        let mut credential_id = vec![0u8; 32];
        for (index, byte) in credential_id.iter_mut().enumerate() {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            *byte = ((nanos >> ((index % 8) * 8)) as u8) ^ (index as u8);
        }
        Ok(Self {
            key,
            credential_id,
            user_id,
            rp_id_hash: sha256(rp_id.as_bytes()),
            counter: 0,
        })
    }

    fn public_coordinates(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        let mut ctx = BigNumContext::new()?;
        let mut x = BigNum::new()?;
        let mut y = BigNum::new()?;
        self.key
            .public_key()
            .affine_coordinates_gfp(&group, &mut x, &mut y, &mut ctx)?;
        Ok((x.to_vec_padded(32)?, y.to_vec_padded(32)?))
    }

    fn cose_public_key(&self) -> Result<Vec<u8>> {
        let (x, y) = self.public_coordinates()?;
        let mut map = BTreeMap::new();
        map.insert(cbor_int(1), cbor_int(2)); // kty: EC2
        map.insert(cbor_int(3), cbor_int(-7)); // alg: ES256
        map.insert(cbor_int(-1), cbor_int(1)); // crv: P-256
        map.insert(cbor_int(-2), cbor_bytes(x));
        map.insert(cbor_int(-3), cbor_bytes(y));
        Ok(serde_cbor_2::to_vec(&serde_cbor_2::Value::Map(map))?)
    }

    fn flags_and_counter(&self, flags: u8, counter: u32) -> Vec<u8> {
        let mut out = self.rp_id_hash.to_vec();
        out.push(flags);
        out.extend_from_slice(&counter.to_be_bytes());
        out
    }

    fn attested_credential_data(&self) -> Result<Vec<u8>> {
        let cose = self.cose_public_key()?;
        let mut out = self.flags_and_counter(0x01 | 0x04 | 0x40, 1);
        out.extend_from_slice(&[0u8; 16]); // aaguid (zeros, attestation none)
        out.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.credential_id);
        out.extend_from_slice(&cose);
        Ok(out)
    }

    fn client_data(&self, kind: &str, challenge: &[u8], origin: &str) -> String {
        serde_json::json!({
            "type": kind,
            "challenge": b64url(challenge),
            "origin": origin,
        })
        .to_string()
    }

    fn attestation_object(&self) -> Result<Vec<u8>> {
        let mut map = BTreeMap::new();
        map.insert(cbor_text("fmt"), cbor_text("none"));
        map.insert(
            cbor_text("attStmt"),
            serde_cbor_2::Value::Map(BTreeMap::new()),
        );
        map.insert(
            cbor_text("authData"),
            cbor_bytes(self.attested_credential_data()?),
        );
        Ok(serde_cbor_2::to_vec(&serde_cbor_2::Value::Map(map))?)
    }

    /// Browser-shaped response to register/finish.
    fn register_response(&self, challenge: &[u8], origin: &str) -> Result<Value> {
        let client_data = self.client_data("webauthn.create", challenge, origin);
        let attestation = self.attestation_object()?;
        Ok(serde_json::json!({
            "id": b64url(&self.credential_id),
            "rawId": b64url(&self.credential_id),
            "type": "public-key",
            "extensions": {},
            "response": {
                "attestationObject": b64url(&attestation),
                "clientDataJSON": b64url(client_data.as_bytes()),
            },
        }))
    }

    fn sign_assertion(&self, auth_data: &[u8], client_data: &[u8]) -> Result<Vec<u8>> {
        let mut signed = auth_data.to_vec();
        signed.extend_from_slice(&sha256(client_data));
        // ECDSA_do_sign takes an already-computed digest.
        let digest = sha256(&signed);
        let sig = EcdsaSig::sign(&digest, self.key.as_ref())?;
        Ok(sig.to_der()?)
    }

    /// Browser-shaped response to login/finish. `counter_override` lets a test
    /// present a rolled-back counter for clone detection.
    fn assertion_response(
        &mut self,
        challenge: &[u8],
        origin: &str,
        counter_override: Option<u32>,
    ) -> Result<Value> {
        let client_data = self.client_data("webauthn.get", challenge, origin);
        let counter = match counter_override {
            Some(value) => value,
            None => {
                self.counter += 2;
                self.counter
            }
        };
        let auth_data = self.flags_and_counter(0x01 | 0x04, counter);
        let signature = self.sign_assertion(&auth_data, client_data.as_bytes())?;
        Ok(serde_json::json!({
            "id": b64url(&self.credential_id),
            "rawId": b64url(&self.credential_id),
            "type": "public-key",
            "extensions": {},
            "response": {
                "authenticatorData": b64url(&auth_data),
                "clientDataJSON": b64url(client_data.as_bytes()),
                "signature": b64url(&signature),
                "userHandle": b64url(&self.user_id),
            },
        }))
    }
}

/// Pull the base64url challenge out of a register/login options envelope.
fn envelope_challenge(body: &Value) -> Vec<u8> {
    let challenge = body["options"]["publicKey"]["challenge"]
        .as_str()
        .expect("options.publicKey.challenge");
    b64url_decode(challenge).expect("challenge base64url")
}

fn json_body(body: &str) -> Value {
    serde_json::from_str(body).expect("json response body")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_list_login_and_rename_full_ceremony() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let addr = hub.addr;
    let origin = format!("http://{ORIGIN_HOST}:{}", addr.port());
    let cookie = bootstrap_login(addr, &bootstrap, &origin).await?;
    let auth = [("Origin", origin.as_str()), ("Cookie", &cookie)];

    // register/start
    let (status, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/start",
        &auth,
        Some(r#"{"name":"My Key"}"#),
    )
    .await?;
    assert_eq!(status, 200, "register/start: {body}");
    let start = json_body(&body);
    let challenge_id = start["challengeId"].as_str().expect("challengeId");
    let challenge = envelope_challenge(&start);
    // The server pins the per-RP fixed v5 user handle, resident key + UV.
    let user_id = b64url_decode(
        start["options"]["publicKey"]["user"]["id"]
            .as_str()
            .unwrap(),
    )?;
    assert_eq!(
        Uuid::from_slice(&user_id)?,
        Uuid::new_v5(&Uuid::NAMESPACE_URL, ORIGIN_HOST.as_bytes())
    );
    assert_eq!(
        start["options"]["publicKey"]["authenticatorSelection"]["residentKey"],
        "required"
    );
    assert_eq!(
        start["options"]["publicKey"]["authenticatorSelection"]["userVerification"],
        "required"
    );

    // register/finish
    let mut authenticator = SoftAuthenticator::new(ORIGIN_HOST, user_id)?;
    let response = authenticator.register_response(&challenge, &origin)?;
    let finish_body =
        serde_json::json!({ "challengeId": challenge_id, "attestation": response }).to_string();
    let (status, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/finish",
        &auth,
        Some(&finish_body),
    )
    .await?;
    assert_eq!(status, 200, "register/finish: {body}");
    let registered = json_body(&body);
    assert_eq!(registered["name"], "My Key");
    assert_eq!(registered["thisDevice"], true);
    let passkey_id = registered["id"].as_str().expect("passkey id").to_string();

    // list
    let (status, _, body) = http(addr, "GET", "/v1/auth/passkeys", &auth, None).await?;
    assert_eq!(status, 200);
    let items = json_body(&body)["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], passkey_id);

    // rename
    let (status, _, _) = http(
        addr,
        "PATCH",
        &format!("/v1/auth/passkeys/{passkey_id}"),
        &auth,
        Some(r#"{"name":"Renamed Key"}"#),
    )
    .await?;
    assert_eq!(status, 200);
    let (_, _, body) = http(addr, "GET", "/v1/auth/passkeys", &auth, None).await?;
    assert_eq!(json_body(&body)["items"][0]["name"], "Renamed Key");

    // login/start (explicit button) then login/finish mints the same cookie.
    let (status, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/login/start",
        &[("Origin", origin.as_str())],
        Some("{}"),
    )
    .await?;
    assert_eq!(status, 200);
    let login_start = json_body(&body);
    assert!(login_start["options"]["mediation"].is_null());
    let login_challenge = envelope_challenge(&login_start);
    let login_challenge_id = login_start["challengeId"].as_str().unwrap();
    let assertion = authenticator.assertion_response(&login_challenge, &origin, None)?;
    let login_finish =
        serde_json::json!({ "challengeId": login_challenge_id, "assertion": assertion })
            .to_string();
    let (status, head, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/login/finish",
        &[("Origin", origin.as_str())],
        Some(&login_finish),
    )
    .await?;
    assert_eq!(status, 200, "login/finish: {body}");
    let session = json_body(&body);
    assert!(session["deviceId"].as_str().is_some());
    assert!(session["token"].as_str().is_some());
    let new_cookie = set_cookie_token(&head).expect("passkey login sets device cookie");
    assert_ne!(new_cookie, cookie, "a fresh device token is minted");

    // The new cookie is a full device session and lastUsedAt got stamped.
    let (status, _, body) = http(
        addr,
        "GET",
        "/v1/auth/passkeys",
        &[("Cookie", &new_cookie)],
        None,
    )
    .await?;
    assert_eq!(status, 200);
    assert!(!json_body(&body)["items"][0]["lastUsedAt"].is_null());
    Ok(())
}

#[tokio::test]
async fn conditional_mediation_is_reflected_in_options() -> Result<()> {
    let (hub, _bootstrap, _dir) = boot().await?;
    let origin = format!("http://{ORIGIN_HOST}:{}", hub.addr.port());
    let (status, _, body) = http(
        hub.addr,
        "POST",
        "/v1/auth/passkeys/login/start",
        &[("Origin", origin.as_str())],
        Some(r#"{"mediation":"conditional"}"#),
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(json_body(&body)["options"]["mediation"], "conditional");
    Ok(())
}

#[tokio::test]
async fn register_requires_a_paired_device() -> Result<()> {
    let (hub, _bootstrap, _dir) = boot().await?;
    let origin = format!("http://{ORIGIN_HOST}:{}", hub.addr.port());
    let (status, _, _) = http(
        hub.addr,
        "POST",
        "/v1/auth/passkeys/register/start",
        &[("Origin", origin.as_str())],
        Some(r#"{"name":"x"}"#),
    )
    .await?;
    assert_eq!(status, 401);
    Ok(())
}

#[tokio::test]
async fn wrong_origin_and_bad_csrf_are_rejected() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let addr = hub.addr;
    let origin = format!("http://{ORIGIN_HOST}:{}", addr.port());
    let cookie = bootstrap_login(addr, &bootstrap, &origin).await?;

    // CSRF: foreign Origin against the loopback Host.
    let (status, _, _) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/start",
        &[
            ("Origin", "http://evil.example.test"),
            ("Host", "irrelevant-ignored"),
            ("Cookie", &cookie),
        ],
        Some(r#"{"name":"x"}"#),
    )
    .await?;
    assert_eq!(status, 403);

    // Start a real ceremony, then answer it claiming a different origin.
    let (_, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/start",
        &[("Origin", origin.as_str()), ("Cookie", &cookie)],
        Some(r#"{"name":"x"}"#),
    )
    .await?;
    let start = json_body(&body);
    let challenge = envelope_challenge(&start);
    let user_id = b64url_decode(
        start["options"]["publicKey"]["user"]["id"]
            .as_str()
            .unwrap(),
    )?;
    let authenticator = SoftAuthenticator::new(ORIGIN_HOST, user_id)?;
    let response = authenticator.register_response(&challenge, "http://other-origin.test")?;
    let payload =
        serde_json::json!({ "challengeId": start["challengeId"], "attestation": response })
            .to_string();
    let (status, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/finish",
        &[("Origin", origin.as_str()), ("Cookie", &cookie)],
        Some(&payload),
    )
    .await?;
    assert_eq!(status, 401, "wrong origin must fail uniformly: {body}");
    Ok(())
}

#[tokio::test]
async fn challenges_are_single_use() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let addr = hub.addr;
    let origin = format!("http://{ORIGIN_HOST}:{}", addr.port());
    let cookie = bootstrap_login(addr, &bootstrap, &origin).await?;

    // Register finish twice: first consumes the challenge, second 401s.
    let (_, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/start",
        &[("Origin", origin.as_str()), ("Cookie", &cookie)],
        Some(r#"{"name":"x"}"#),
    )
    .await?;
    let start = json_body(&body);
    let challenge = envelope_challenge(&start);
    let user_id = b64url_decode(
        start["options"]["publicKey"]["user"]["id"]
            .as_str()
            .unwrap(),
    )?;
    let authenticator = SoftAuthenticator::new(ORIGIN_HOST, user_id)?;
    let response = authenticator.register_response(&challenge, &origin)?;
    let payload =
        serde_json::json!({ "challengeId": start["challengeId"], "attestation": response })
            .to_string();
    let (first, _, _) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/finish",
        &[("Origin", origin.as_str()), ("Cookie", &cookie)],
        Some(&payload),
    )
    .await?;
    assert_eq!(first, 200);
    let (second, _, _) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/finish",
        &[("Origin", origin.as_str()), ("Cookie", &cookie)],
        Some(&payload),
    )
    .await?;
    assert_eq!(second, 401);
    Ok(())
}

#[tokio::test]
async fn duplicate_credential_id_conflicts() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let addr = hub.addr;
    let origin = format!("http://{ORIGIN_HOST}:{}", addr.port());
    let cookie = bootstrap_login(addr, &bootstrap, &origin).await?;
    let auth = [("Origin", origin.as_str()), ("Cookie", cookie.as_str())];

    async fn register_start(
        addr: std::net::SocketAddr,
        auth: &[(&str, &str)],
        name: &str,
    ) -> Value {
        let (_, _, body) = http(
            addr,
            "POST",
            "/v1/auth/passkeys/register/start",
            auth,
            Some(&format!(r#"{{"name":"{name}"}}"#)),
        )
        .await
        .unwrap();
        json_body(&body)
    }

    let first = register_start(addr, &auth, "first").await;
    let user_id = b64url_decode(
        first["options"]["publicKey"]["user"]["id"]
            .as_str()
            .unwrap(),
    )?;
    let authenticator = SoftAuthenticator::new(ORIGIN_HOST, user_id)?;
    let challenge = envelope_challenge(&first);
    let response = authenticator.register_response(&challenge, &origin)?;
    let payload =
        serde_json::json!({ "challengeId": first["challengeId"], "attestation": response })
            .to_string();
    let (status, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/finish",
        &auth,
        Some(&payload),
    )
    .await?;
    assert_eq!(status, 200, "first registration: {body}");

    // Same authenticator, fresh ceremony -> unique credential id rejects.
    let second = register_start(addr, &auth, "second").await;
    let challenge = envelope_challenge(&second);
    let response = authenticator.register_response(&challenge, &origin)?;
    let payload =
        serde_json::json!({ "challengeId": second["challengeId"], "attestation": response })
            .to_string();
    let (status, _, _) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/finish",
        &auth,
        Some(&payload),
    )
    .await?;
    assert_eq!(status, 409);
    Ok(())
}

#[tokio::test]
async fn deleted_passkey_and_counter_rollback_fail_login() -> Result<()> {
    let (hub, bootstrap, _dir) = boot().await?;
    let addr = hub.addr;
    let origin = format!("http://{ORIGIN_HOST}:{}", addr.port());
    let cookie = bootstrap_login(addr, &bootstrap, &origin).await?;
    let auth = [("Origin", origin.as_str()), ("Cookie", cookie.as_str())];

    // Register.
    let (_, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/start",
        &auth,
        Some(r#"{"name":"x"}"#),
    )
    .await?;
    let start = json_body(&body);
    let user_id = b64url_decode(
        start["options"]["publicKey"]["user"]["id"]
            .as_str()
            .unwrap(),
    )?;
    let mut authenticator = SoftAuthenticator::new(ORIGIN_HOST, user_id)?;
    let challenge = envelope_challenge(&start);
    let response = authenticator.register_response(&challenge, &origin)?;
    let payload =
        serde_json::json!({ "challengeId": start["challengeId"], "attestation": response })
            .to_string();
    let (_, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/register/finish",
        &auth,
        Some(&payload),
    )
    .await?;
    let passkey_id = json_body(&body)["id"].as_str().unwrap().to_string();

    // One healthy login (counter 2 > stored 1).
    let (_, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/login/start",
        &[("Origin", origin.as_str())],
        Some("{}"),
    )
    .await?;
    let login = json_body(&body);
    let assertion = authenticator.assertion_response(&envelope_challenge(&login), &origin, None)?;
    let payload =
        serde_json::json!({ "challengeId": login["challengeId"], "assertion": assertion })
            .to_string();
    let (status, _, _) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/login/finish",
        &[("Origin", origin.as_str())],
        Some(&payload),
    )
    .await?;
    assert_eq!(status, 200);

    // Rolled-back/equal counter with the prior challenge's successor fails.
    let (_, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/login/start",
        &[("Origin", origin.as_str())],
        Some("{}"),
    )
    .await?;
    let clone_login = json_body(&body);
    let bad_assertion =
        authenticator.assertion_response(&envelope_challenge(&clone_login), &origin, Some(0))?;
    let payload = serde_json::json!({
        "challengeId": clone_login["challengeId"],
        "assertion": bad_assertion,
    })
    .to_string();
    let (status, _, _) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/login/finish",
        &[("Origin", origin.as_str())],
        Some(&payload),
    )
    .await?;
    assert_eq!(status, 401, "counter rollback is a clone signal");

    // Delete the key; a fresh assertion is now unknown.
    let (status, _, _) = http(
        addr,
        "DELETE",
        &format!("/v1/auth/passkeys/{passkey_id}"),
        &auth,
        None,
    )
    .await?;
    assert_eq!(status, 200);
    let (_, _, body) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/login/start",
        &[("Origin", origin.as_str())],
        Some("{}"),
    )
    .await?;
    let after_delete = json_body(&body);
    let assertion =
        authenticator.assertion_response(&envelope_challenge(&after_delete), &origin, None)?;
    let payload = serde_json::json!({
        "challengeId": after_delete["challengeId"],
        "assertion": assertion,
    })
    .to_string();
    let (status, _, _) = http(
        addr,
        "POST",
        "/v1/auth/passkeys/login/finish",
        &[("Origin", origin.as_str())],
        Some(&payload),
    )
    .await?;
    assert_eq!(status, 401);
    Ok(())
}

// Keep Durations referenced if future tests add time-based assertions.
#[allow(dead_code)]
const _TIMEOUT: Duration = Duration::from_secs(10);
