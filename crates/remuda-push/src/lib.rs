//! Web Push for the Remuda Hub.
//!
//! Native sessions stay the resume authority; this crate only wakes a device
//! that already subscribed. The Hub mounts [`router`] at `/push`.
//!
//! Behaviour follows herdrx `internal/push` (MIT, rewritten): VAPID file
//! `vapid.json` mode 0600, subscription upsert by endpoint, prune on 404/410.

#![forbid(unsafe_code)]

mod error;
mod http;
mod notify;
mod store;
mod tag;
mod validate;
mod vapid;

use crate::notify::Sender;
use crate::store::Store;
use crate::validate::ValidateOpts;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub use error::Error;
pub use http::{ConfigResponse, SubscribeKeys, SubscribeRequest, UnsubscribeRequest, router};
pub use notify::{Delivery, Notification, ReqwestTransport, Transport, payload_bytes};
pub use store::Subscription;
pub use tag::PushTag;
pub use validate::validate_subscription;

/// Hub-facing push service: keys, SQLite subscriptions, encrypted send.
#[derive(Clone)]
pub struct PushService {
    sender: Arc<Sender>,
    validate_opts: ValidateOpts,
}

/// Tunables for [`PushService::open_with`].
pub struct OpenOptions {
    /// TTL seconds advertised to the push service (herdrx uses 300).
    pub ttl: u32,
    /// Extra attempts after the first failure.
    pub max_retries: u32,
    /// Base backoff; multiplied by the attempt number.
    pub backoff: Duration,
    /// Allow loopback HTTP endpoints (tests only).
    pub allow_loopback: bool,
    /// Outbound transport.
    pub transport: Arc<dyn Transport>,
}

impl OpenOptions {
    /// Production defaults: 300s TTL, 3 retries, 200ms backoff, reqwest.
    pub fn production() -> Result<Self, Error> {
        Ok(Self {
            ttl: 300,
            max_retries: 3,
            backoff: Duration::from_millis(200),
            allow_loopback: false,
            transport: Arc::new(ReqwestTransport::new()?),
        })
    }

    /// Test defaults: loopback allowed, no sleep, caller-supplied transport.
    pub fn test(transport: Arc<dyn Transport>) -> Self {
        Self {
            ttl: 300,
            max_retries: 2,
            backoff: Duration::from_millis(0),
            allow_loopback: true,
            transport,
        }
    }
}

impl PushService {
    /// Open (or create) `vapid.json` and `push.sqlite` under `data_dir`.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with(data_dir, OpenOptions::production()?)
    }

    /// Open with an explicit transport (mock HTTP in tests).
    pub fn open_with(data_dir: impl AsRef<Path>, options: OpenOptions) -> Result<Self, Error> {
        let data_dir = data_dir.as_ref();
        let (keys, _) = vapid::load_or_create(data_dir)?;
        let store = Store::open(data_dir)?;
        Ok(Self {
            sender: Arc::new(Sender::new(
                keys,
                store,
                options.transport,
                options.ttl,
                options.max_retries,
                options.backoff,
            )),
            validate_opts: ValidateOpts {
                allow_loopback: options.allow_loopback,
            },
        })
    }

    /// Application-server public key for `pushManager.subscribe`.
    pub fn public_key(&self) -> &str {
        self.sender.public_key()
    }

    pub(crate) fn validate_opts(&self) -> ValidateOpts {
        self.validate_opts
    }

    /// Insert or replace a subscription (unique on endpoint).
    pub fn upsert(&self, subscription: Subscription) -> Result<(), Error> {
        validate_subscription(
            &subscription.endpoint,
            &subscription.p256dh,
            &subscription.auth,
            self.validate_opts,
        )?;
        self.sender.store().upsert(&subscription)
    }

    /// Lookup by endpoint.
    pub fn subscription(&self, endpoint: &str) -> Result<Option<Subscription>, Error> {
        self.sender.store().get(endpoint)
    }

    /// All stored subscriptions.
    pub fn subscriptions(&self) -> Result<Vec<Subscription>, Error> {
        self.sender.store().list()
    }

    /// Subscriptions for one device.
    pub fn subscriptions_for_device(&self, device_id: &str) -> Result<Vec<Subscription>, Error> {
        self.sender.store().list_device(device_id)
    }

    /// Delete by endpoint. Returns whether a row was removed.
    pub fn unsubscribe(&self, endpoint: &str) -> Result<bool, Error> {
        self.sender.store().delete_endpoint(endpoint)
    }

    /// Encrypt + send; prune the row on 404/410.
    pub async fn notify(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        tag: &PushTag,
    ) -> Result<Delivery, Error> {
        self.sender
            .notify_and_prune(subscription, notification, tag)
            .await
    }

    /// Encrypt without sending (tests / inspection).
    pub fn encrypt(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        tag: &PushTag,
    ) -> Result<web_push::WebPushMessage, Error> {
        self.sender
            .encrypt(subscription, notification, Some(&tag.topic()))
    }
}

/// Construct a new subscription identity (`push_` + UUID).
pub fn new_subscription_id() -> String {
    crate::store::new_id()
}
