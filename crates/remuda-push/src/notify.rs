//! Encrypt (RFC 8291 / aes128gcm) and deliver with retry/backoff.

use crate::Error;
use crate::store::{Store, Subscription};
use crate::tag::PushTag;
use crate::vapid::KeysFile;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use web_push::{
    ContentEncoding, SubscriptionInfo, Urgency, VapidSignatureBuilder, WebPushMessage,
    WebPushMessageBuilder, WebPushPayload,
};

/// JSON payload shown by the service worker.
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    /// Short title.
    pub title: String,
    /// Body text.
    pub body: String,
    /// Client-side collapse tag (`interaction:{id}` / `instance:{id}`).
    pub tag: String,
    /// Deep-link payload.
    pub data: HashMap<String, String>,
}

impl Notification {
    /// Build a Hub notification with `data.url` and a typed tag.
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        tag: PushTag,
        url: impl Into<String>,
    ) -> Self {
        let mut data = HashMap::new();
        data.insert("url".into(), url.into());
        Self {
            title: title.into(),
            body: body.into(),
            tag: tag.as_str(),
            data,
        }
    }
}

/// Result of delivering to one subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Push service accepted the message.
    Sent,
    /// 404/410 — caller should prune.
    Gone,
}

/// Outbound HTTP used after encryption. Tests inject a mock.
pub trait Transport: Send + Sync {
    /// POST `body` to `url` with `headers`. Returns the HTTP status.
    fn post(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u16, Error>> + Send + '_>>;
}

/// reqwest-backed transport for production.
#[derive(Debug, Clone, Default)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// 10s timeout, no redirects (untrusted Location).
    pub fn new() -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Delivery)?;
        Ok(Self { client })
    }
}

impl Transport for ReqwestTransport {
    fn post(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u16, Error>> + Send + '_>> {
        let client = self.client.clone();
        Box::pin(async move {
            let mut req = client.post(&url).body(body);
            for (name, value) in headers {
                req = req.header(name, value);
            }
            let response = req.send().await.map_err(|_| Error::Delivery)?;
            Ok(response.status().as_u16())
        })
    }
}

pub(crate) struct Sender {
    keys: KeysFile,
    store: Store,
    transport: Arc<dyn Transport + Send + Sync>,
    ttl: u32,
    max_retries: u32,
    backoff: Duration,
}

impl Sender {
    pub(crate) fn new(
        keys: KeysFile,
        store: Store,
        transport: Arc<dyn Transport + Send + Sync>,
        ttl: u32,
        max_retries: u32,
        backoff: Duration,
    ) -> Self {
        Self {
            keys,
            store,
            transport,
            ttl,
            max_retries,
            backoff,
        }
    }

    pub(crate) fn public_key(&self) -> &str {
        &self.keys.public
    }

    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// Encrypt using the web-push crate (RFC 8291 aes128gcm) without sending.
    pub(crate) fn encrypt(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        topic: Option<&str>,
    ) -> Result<WebPushMessage, Error> {
        let payload = serde_json::to_vec(notification)?;
        let info = SubscriptionInfo::new(
            subscription.endpoint.clone(),
            subscription.p256dh.clone(),
            subscription.auth.clone(),
        );
        let mut sig = VapidSignatureBuilder::from_base64(&self.keys.private, &info)?;
        sig.add_claim("sub", "mailto:admin@localhost");
        let signature = sig.build()?;
        let mut builder = WebPushMessageBuilder::new(&info);
        builder.set_payload(ContentEncoding::Aes128Gcm, &payload);
        builder.set_vapid_signature(signature);
        builder.set_ttl(self.ttl);
        builder.set_urgency(Urgency::High);
        if let Some(topic) = topic {
            builder.set_topic(topic.to_owned());
        }
        Ok(builder.build()?)
    }

    pub(crate) async fn notify(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        tag: &PushTag,
    ) -> Result<Delivery, Error> {
        let topic = tag.topic();
        let mut last = Error::Delivery;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                tokio::time::sleep(self.backoff * attempt).await;
            }
            match self
                .send_once(subscription, notification, Some(&topic))
                .await
            {
                Ok(()) => return Ok(Delivery::Sent),
                Err(Error::Gone) => return Ok(Delivery::Gone),
                Err(Error::Status(code)) if is_retryable(code) => {
                    last = Error::Status(code);
                }
                Err(Error::Delivery) => {
                    last = Error::Delivery;
                }
                Err(other) => return Err(other),
            }
        }
        Err(last)
    }

    pub(crate) async fn notify_and_prune(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        tag: &PushTag,
    ) -> Result<Delivery, Error> {
        let outcome = self.notify(subscription, notification, tag).await?;
        if outcome == Delivery::Gone {
            self.store.delete_endpoint(&subscription.endpoint)?;
        }
        Ok(outcome)
    }

    async fn send_once(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        topic: Option<&str>,
    ) -> Result<(), Error> {
        let message = self.encrypt(subscription, notification, topic)?;
        let url = message.endpoint.to_string();
        let mut headers = Vec::new();
        headers.push(("TTL".into(), message.ttl.to_string()));
        if let Some(urgency) = message.urgency {
            headers.push(("Urgency".into(), urgency.to_string()));
        }
        if let Some(topic) = &message.topic {
            headers.push(("Topic".into(), topic.clone()));
        }
        let body = match message.payload {
            Some(WebPushPayload {
                content,
                crypto_headers,
                content_encoding,
            }) => {
                headers.push((
                    "Content-Encoding".into(),
                    match content_encoding {
                        ContentEncoding::Aes128Gcm => "aes128gcm".into(),
                        ContentEncoding::AesGcm => "aesgcm".into(),
                    },
                ));
                for (name, value) in crypto_headers {
                    headers.push((name.to_owned(), value));
                }
                content
            }
            None => Vec::new(),
        };
        let status = self.transport.post(url, headers, body).await?;
        match status {
            200..=202 => Ok(()),
            404 | 410 => Err(Error::Gone),
            other => Err(Error::Status(other)),
        }
    }
}

fn is_retryable(code: u16) -> bool {
    code == 429 || (500..600).contains(&code)
}

/// Expose encrypted bytes for tests.
pub fn payload_bytes(message: &WebPushMessage) -> Option<&[u8]> {
    message.payload.as_ref().map(|p| p.content.as_slice())
}
