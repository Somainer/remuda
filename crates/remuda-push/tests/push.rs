//! Key persistence, subscription CRUD, RFC 8291 round-trip, and 410 pruning.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use remuda_push::{
    Delivery, Notification, OpenOptions, PushService, PushTag, ReqwestTransport, Subscription,
    Transport, new_subscription_id, payload_bytes, router,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::TempDir;
use tower::ServiceExt;

struct StatusTransport {
    status: u16,
    hits: Mutex<u32>,
}

impl Transport for StatusTransport {
    fn post(
        &self,
        _url: String,
        _headers: Vec<(String, String)>,
        _body: Vec<u8>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<u16, remuda_push::Error>> + Send + '_>,
    > {
        *self.hits.lock().expect("hits") += 1;
        let status = self.status;
        Box::pin(async move { Ok(status) })
    }
}

fn sample_keys() -> (ece::crypto::EcKeyComponents, [u8; 16], String, String) {
    let (pair, auth) = ece::generate_keypair_and_auth_secret().expect("ece keygen");
    let components = pair.raw_components().expect("components");
    let p256dh = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pair.pub_as_raw().expect("pub"))
    };
    let auth_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(auth)
    };
    (components, auth, p256dh, auth_b64)
}

fn sub(endpoint: &str, p256dh: &str, auth: &str) -> Subscription {
    Subscription {
        id: new_subscription_id(),
        device_id: "dev-1".into(),
        endpoint: endpoint.into(),
        p256dh: p256dh.into(),
        auth: auth.into(),
    }
}

#[test]
fn vapid_keys_persist_mode_0600() {
    let dir = TempDir::new().unwrap();
    let first = PushService::open(dir.path()).unwrap();
    let path = dir.path().join("vapid.json");
    let meta = std::fs::metadata(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
    let pk = first.public_key().to_owned();
    assert!(!pk.is_empty());
    drop(first);
    let second = PushService::open(dir.path()).unwrap();
    assert_eq!(second.public_key(), pk);
}

#[test]
fn subscription_crud() {
    let dir = TempDir::new().unwrap();
    let transport = Arc::new(StatusTransport {
        status: 201,
        hits: Mutex::new(0),
    });
    let svc = PushService::open_with(dir.path(), OpenOptions::test(transport)).unwrap();
    let (_, _, p256dh, auth) = sample_keys();
    let endpoint = "https://push.example.test/sub-a";
    let s = sub(endpoint, &p256dh, &auth);
    svc.upsert(s.clone()).unwrap();
    assert_eq!(svc.subscriptions().unwrap().len(), 1);
    assert_eq!(svc.subscriptions_for_device("dev-1").unwrap().len(), 1);
    svc.upsert(Subscription {
        id: new_subscription_id(),
        device_id: "dev-2".into(),
        endpoint: endpoint.into(),
        p256dh: p256dh.clone(),
        auth: auth.clone(),
    })
    .unwrap();
    let listed = svc.subscriptions().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].device_id, "dev-2");
    assert!(svc.unsubscribe(endpoint).unwrap());
    assert!(svc.subscriptions().unwrap().is_empty());
}

#[test]
fn encrypt_round_trip_rfc8291() {
    let dir = TempDir::new().unwrap();
    let transport = Arc::new(StatusTransport {
        status: 201,
        hits: Mutex::new(0),
    });
    let svc = PushService::open_with(dir.path(), OpenOptions::test(transport)).unwrap();
    let (components, auth_bytes, p256dh, auth) = sample_keys();
    let s = sub("https://push.example.test/ece", &p256dh, &auth);
    let tag = PushTag::Interaction {
        id: "int_01993ab0-0000-7000-8000-000000000001".into(),
    };
    let notification = Notification::new("Need input", "Approve Bash?", tag.clone(), "/approvals");
    let message = svc.encrypt(&s, &notification, &tag).unwrap();
    assert_eq!(message.ttl, 300);
    assert_eq!(message.topic.as_deref().map(str::len), Some(32));
    let cipher = payload_bytes(&message).expect("payload");
    let plain = ece::decrypt(&components, &auth_bytes, cipher).expect("decrypt");
    let value: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(value["title"], "Need input");
    assert_eq!(
        value["tag"],
        "interaction:int_01993ab0-0000-7000-8000-000000000001"
    );
    assert_eq!(value["data"]["url"], "/approvals");
}

#[tokio::test]
async fn prune_on_410_mock_http_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/gone", axum::routing::post(|| async { StatusCode::GONE }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = TempDir::new().unwrap();
    let transport = Arc::new(ReqwestTransport::new().unwrap());
    let mut opts = OpenOptions::test(transport);
    opts.max_retries = 0;
    let svc = PushService::open_with(dir.path(), opts).unwrap();
    let (_, _, p256dh, auth) = sample_keys();
    let endpoint = format!("http://127.0.0.1:{}/gone", addr.port());
    let s = sub(&endpoint, &p256dh, &auth);
    svc.upsert(s.clone()).unwrap();
    assert_eq!(svc.subscriptions().unwrap().len(), 1);
    let tag = PushTag::Instance {
        id: "ins_01993ab0-0000-7000-8000-000000000001".into(),
    };
    let notification = Notification::new("Exited", "run failed", tag.clone(), "/sessions");
    let outcome = svc.notify(&s, &notification, &tag).await.unwrap();
    assert_eq!(outcome, Delivery::Gone);
    assert!(svc.subscriptions().unwrap().is_empty());
}

#[tokio::test]
async fn axum_config_subscribe_delete() {
    let dir = TempDir::new().unwrap();
    let transport = Arc::new(StatusTransport {
        status: 201,
        hits: Mutex::new(0),
    });
    let svc = PushService::open_with(dir.path(), OpenOptions::test(transport)).unwrap();
    let pk = svc.public_key().to_owned();
    let app = Router::new().nest("/push", router(svc.clone()));
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/push/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let cfg: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(cfg["public_key"], pk);

    let (_, _, p256dh, auth) = sample_keys();
    let endpoint = "https://push.example.test/browser";
    let body = json!({
        "endpoint": endpoint,
        "keys": { "p256dh": p256dh, "auth": auth },
        "deviceId": "phone"
    });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/push/subscriptions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    assert_eq!(svc.subscriptions_for_device("phone").unwrap().len(), 1);

    let encoded = urlencoding_lite(endpoint);
    let res = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/push/subscriptions?endpoint={encoded}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(svc.subscriptions().unwrap().is_empty());
}

fn urlencoding_lite(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
