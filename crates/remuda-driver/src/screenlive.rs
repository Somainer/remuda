//! Building the screen-tier live-status observation from a parsed spinner line.
//!
//! Both PTY carriers share one payload shape: a `turn`/`live.status` native
//! lifecycle whose `related_ids` carry the spinner verb, the streamed token
//! estimate, the phrase, an interruptible bit, and a `since` anchor re-derived
//! from the screen's own elapsed. It is *status only* — the spinner line the
//! TUI paints (live-view design §2.4); never message or tool content.
//!
//! The observation rides the same channel as the carrier's `agent_status`
//! evidence (`pty` for native, `herdr` for the remote pane) but is tagged
//! `tier=screen`, which is the key the web projection selects on. At most one
//! is journaled per distinct reading ([`remuda_screen::ScreenLiveLatch`]).

use remuda_protocol::{
    Knowledge, LifecyclePayload, LifecycleTopic, NativeLifecycle, ObservationPayload, Severity,
};
use remuda_screen::ScreenLive;
use std::collections::BTreeMap;
use time::{Duration, OffsetDateTime};

/// `native_name` of the screen-tier live status lifecycle.
pub(crate) const LIVE_STATUS_NAME: &str = "live.status";

/// Tag whose presence marks a live-status lifecycle. `"0"` clears the strip.
pub(crate) const KEY_LIVE: &str = "liveStatus";
/// Spinner verb without the ellipsis (`"Razzmatazzing"`).
pub(crate) const KEY_VERB: &str = "verb";
/// The trailing spinner phrase (`"thinking with xhigh effort"`).
pub(crate) const KEY_PHRASE: &str = "phrase";
/// Screen token label as printed (`"66.0k"`).
pub(crate) const KEY_TOKENS_LABEL: &str = "tokensLabel";
/// Screen token estimate as a count.
pub(crate) const KEY_TOKENS_DOWN: &str = "tokensDown";
/// Screen elapsed as printed (`"49m 38s"`).
pub(crate) const KEY_ELAPSED_SCREEN: &str = "elapsedScreen";
/// `"1"` while the TUI offers esc-to-interrupt.
pub(crate) const KEY_INTERRUPTIBLE: &str = "interruptible";

fn rfc3339(at: OffsetDateTime) -> String {
    let date = at.date();
    let (hour, minute, second) = at.time().as_hms();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        hour,
        minute,
        second,
        at.millisecond(),
    )
}

fn base_tags() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("tier".into(), "screen".into()),
        ("provision".into(), "emulated".into()),
    ])
}

fn payload(related: BTreeMap<String, String>) -> ObservationPayload {
    ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        NativeLifecycle {
            topic: LifecycleTopic::Turn,
            native_name: LIVE_STATUS_NAME.into(),
            native_id: Knowledge::NotApplicable,
            status: Knowledge::NotApplicable,
            related_ids: related,
            data_ref: None,
            severity: Severity::Info,
            affects_completion: false,
        },
    ))))
}

/// Build the live observation for one spinner reading.
#[must_use]
pub(crate) fn live_status_payload(live: &ScreenLive, now: OffsetDateTime) -> ObservationPayload {
    let mut related = base_tags();
    related.insert(KEY_LIVE.into(), "1".into());
    related.insert(KEY_VERB.into(), live.verb.clone());
    if let Some(phrase) = &live.phrase {
        related.insert(KEY_PHRASE.into(), phrase.clone());
    }
    if let Some(tokens) = &live.tokens {
        related.insert(KEY_TOKENS_LABEL.into(), tokens.label.clone());
        if let Some(count) = tokens.count {
            related.insert(KEY_TOKENS_DOWN.into(), count.to_string());
        }
    }
    if let Some(elapsed) = &live.elapsed {
        related.insert(KEY_ELAPSED_SCREEN.into(), elapsed.text.clone());
        // Re-anchor: the browser's clock keeps owning elapsed; this anchor only
        // corrects drift (`now − the elapsed the screen itself printed`).
        if let Some(ms) = elapsed.ms {
            // Re-anchor: the browser's clock keeps owning elapsed; this anchor only
            // corrects drift (`now − the elapsed the screen itself printed`).
            let back = Duration::milliseconds(i64::try_from(ms).unwrap_or(i64::MAX));
            related.insert("since".into(), rfc3339(now - back));
        }
    }
    if live.interruptible {
        related.insert(KEY_INTERRUPTIBLE.into(), "1".into());
    }
    payload(related)
}

/// The clear observation, emitted once when the spinner leaves the screen.
#[must_use]
pub(crate) fn live_status_inactive() -> ObservationPayload {
    let mut related = base_tags();
    related.insert(KEY_LIVE.into(), "0".into());
    payload(related)
}

/// Current UTC, matching the RFC3339-ms shape `now_ts` produces.
#[must_use]
pub(crate) fn now_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::LifecyclePayload;
    use remuda_screen::{ScreenGrid, screen_live};

    fn parse(line: &str) -> ScreenLive {
        screen_live(&ScreenGrid::from_lines([line])).expect("status line parses")
    }

    #[test]
    fn active_payload_carries_every_field_at_screen_tier() {
        let live = parse(
            "· Razzmatazzing… (49m 38s · ↓ 66.0k tokens · thinking some more with xhigh effort)",
        );
        let now = OffsetDateTime::from_unix_timestamp_nanos(1_789_000_000_000_000_000).unwrap();
        let payload = live_status_payload(&live, now);
        let ObservationPayload::Lifecycle(boxed) = payload else {
            panic!("not a lifecycle");
        };
        let LifecyclePayload::Native(native) = *boxed else {
            panic!("not native");
        };
        assert_eq!(native.topic, LifecycleTopic::Turn);
        assert_eq!(native.native_name, LIVE_STATUS_NAME);
        let tags = &native.related_ids;
        assert_eq!(tags.get("tier").map(String::as_str), Some("screen"));
        assert_eq!(tags.get("provision").map(String::as_str), Some("emulated"));
        assert_eq!(tags.get(KEY_LIVE).map(String::as_str), Some("1"));
        assert_eq!(
            tags.get(KEY_VERB).map(String::as_str),
            Some("Razzmatazzing")
        );
        assert_eq!(
            tags.get(KEY_TOKENS_LABEL).map(String::as_str),
            Some("66.0k")
        );
        assert_eq!(tags.get(KEY_TOKENS_DOWN).map(String::as_str), Some("66000"));
        assert_eq!(
            tags.get(KEY_ELAPSED_SCREEN).map(String::as_str),
            Some("49m 38s")
        );
        assert!(
            tags.get(KEY_PHRASE)
                .is_some_and(|p| p.contains("xhigh effort"))
        );
        assert!(tags.get(KEY_INTERRUPTIBLE).is_none());
        // The re-anchor is `now − printed elapsed`.
        let since = time::OffsetDateTime::parse(
            tags.get("since").expect("since anchor"),
            &time::format_description::well_known::Rfc3339,
        )
        .expect("rfc3339");
        let delta = (now - since).whole_seconds();
        assert_eq!(delta, 49 * 60 + 38);
    }

    #[test]
    fn interruptible_reading_tags_the_esc_hint() {
        let live = parse("✳ Thinking… (esc to interrupt)");
        assert!(live.interruptible, "the legacy hint is parsed");
        let payload = live_status_payload(&live, now_utc());
        let ObservationPayload::Lifecycle(boxed) = payload else {
            panic!("not a lifecycle");
        };
        let LifecyclePayload::Native(native) = *boxed else {
            panic!("not native");
        };
        assert_eq!(
            native
                .related_ids
                .get(KEY_INTERRUPTIBLE)
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn inactive_payload_clears_the_strip_and_keeps_the_tier() {
        let ObservationPayload::Lifecycle(boxed) = live_status_inactive() else {
            panic!("not a lifecycle");
        };
        let LifecyclePayload::Native(native) = *boxed else {
            panic!("not native");
        };
        assert_eq!(native.native_name, LIVE_STATUS_NAME);
        assert_eq!(
            native.related_ids.get(KEY_LIVE).map(String::as_str),
            Some("0")
        );
        assert_eq!(
            native.related_ids.get("tier").map(String::as_str),
            Some("screen")
        );
        assert!(!native.related_ids.contains_key(KEY_VERB));
    }
}
