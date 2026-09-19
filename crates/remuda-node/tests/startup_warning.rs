//! Start-up signals for a long data directory (brief item D): the one-time
//! redirect warning and the dead-runtime-socket sweep.
#![cfg(unix)]

use std::fmt::Debug;
use std::sync::Mutex;

use remuda_node::{DevServerConfig, LocalDrivers, ServeConfig, compose};
use tracing::{
    Level, Subscriber,
    field::{Field, Visit},
};

/// One captured event: the debug-formatted fields (level/target are used by
/// the subscriber's `enabled` filter and in panic output via Debug).
#[derive(Debug)]
struct CapturedEvent {
    #[allow(dead_code)]
    level: Level,
    #[allow(dead_code)]
    target: String,
    fields: Vec<(String, String)>,
}

struct CaptureSubscriber {
    events: Mutex<Vec<CapturedEvent>>,
}

struct EventVisitor<'a>(&'a mut Vec<(String, String)>);

impl Visit for EventVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        // Only the crate's own warnings; other targets and levels are noise.
        metadata.target().starts_with("remuda_node") && metadata.level() == &Level::WARN
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::Id {
        tracing::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut captured = CapturedEvent {
            level: event.metadata().level().to_owned(),
            target: event.metadata().target().to_owned(),
            fields: Vec::new(),
        };
        event.record(&mut EventVisitor(&mut captured.fields));
        self.events.lock().unwrap().push(captured);
    }

    fn enter(&self, _id: &tracing::Id) {}

    fn exit(&self, _id: &tracing::Id) {}
}

fn fake_config(data_dir: std::path::PathBuf) -> ServeConfig {
    ServeConfig {
        http: DevServerConfig::loopback(0)
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        data_dir,
        drivers: LocalDrivers::Fake,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_data_dir_warns_exactly_once_with_lengths_and_limit() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("d".repeat(90)).join("node-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    assert!(data_dir.as_os_str().len() > 90);

    let subscriber = std::sync::Arc::new(CaptureSubscriber {
        events: Mutex::new(Vec::new()),
    });
    let guard = tracing::subscriber::set_default::<std::sync::Arc<CaptureSubscriber>>(
        std::sync::Arc::clone(&subscriber),
    );
    let node = compose(&fake_config(data_dir)).expect("compose");
    drop(guard);

    let events = subscriber.events.lock().unwrap();
    let warnings: Vec<_> = events
        .iter()
        .filter(|event| event.fields.iter().any(|(name, _)| name == "data_dir_len"))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "exactly one redirect warning: {events:?}"
    );
    let fields = &warnings[0].fields;
    assert!(
        fields.iter().any(|(name, value)| name == "data_dir_len"
            && value.parse::<usize>().is_ok_and(|len| len > 90)),
        "{fields:?}"
    );
    assert!(
        fields
            .iter()
            .any(|(name, value)| name == "safe_limit" && value.parse::<usize>().is_ok()),
        "{fields:?}"
    );
    drop(node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_data_dir_emits_no_redirect_warning() {
    // tempfile roots here sit under a 44-byte path, which leaves the
    // representative instance socket at 105 bytes and *does* redirect. The
    // negative case therefore needs a genuinely short root: data_dir must be
    // at most 39 bytes for the 61-byte `/instances/<id>/hook.sock` suffix to
    // stay within the 100-byte safe threshold.
    let data_dir =
        std::path::PathBuf::from(format!("/tmp/remuda-warn-short-{}", std::process::id()));
    std::fs::create_dir_all(&data_dir).unwrap();
    assert!(data_dir.as_os_str().len() <= 39, "{}", data_dir.display());

    let subscriber = std::sync::Arc::new(CaptureSubscriber {
        events: Mutex::new(Vec::new()),
    });
    let guard = tracing::subscriber::set_default::<std::sync::Arc<CaptureSubscriber>>(
        std::sync::Arc::clone(&subscriber),
    );
    let node = compose(&fake_config(data_dir.clone())).expect("compose");
    drop(guard);

    let events = subscriber.events.lock().unwrap();
    assert!(
        events
            .iter()
            .all(|event| event.fields.iter().all(|(name, _)| name != "data_dir_len")),
        "no redirect warning for a short data dir: {events:?}"
    );
    drop(node);
    let _ = std::fs::remove_dir_all(&data_dir);
}
