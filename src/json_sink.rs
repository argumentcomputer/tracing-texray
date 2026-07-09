//! Machine-readable per-span timing sink.
//!
//! The human timeline and the `streaming` one-liners go to a writer for eyes;
//! this sink writes one JSON object per closed span to a file, as JSON Lines:
//!
//! ```json
//! {"span":"aiur/prove","seconds":1.234567}
//! ```
//!
//! so a benchmark harness can aggregate per-phase timings (e.g. render a
//! collapsible `execute` vs `witness` vs `prove` breakdown) without scraping
//! the formatted timeline. Off unless [`to_file`] has been called.

use std::fs::{File, OpenOptions};
use std::io::Write;

use parking_lot::Mutex;

static SINK: Mutex<Option<File>> = Mutex::new(None);

/// Send per-span timing records to `path` as JSON Lines, truncating any
/// existing file. Idempotent-ish: the most recent call wins.
pub fn to_file(path: &str) -> std::io::Result<()> {
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    *SINK.lock() = Some(f);
    Ok(())
}

/// Whether a sink is active (lets the tracker skip work when it isn't).
pub(crate) fn is_active() -> bool {
    SINK.lock().is_some()
}

/// Record a phase timing directly, without a tracing span. For consumers that
/// already measured a phase by hand (e.g. a zkVM host timing its `execute` /
/// `prove` calls, where installing a full TeXRay subscriber would fight the
/// SDK's own global logger) and want it in the same per-phase stream as
/// span-derived timings. No-op when no sink is set.
pub fn record_manual(name: &str, seconds: f64) {
    record(name, seconds);
}

/// Append one span's timing as a JSON line. No-op when no sink is set.
pub(crate) fn record(name: &str, seconds: f64) {
    let mut guard = SINK.lock();
    if let Some(f) = guard.as_mut() {
        // Span names are `&'static str` identifiers in practice, but escape the
        // two JSON-significant characters defensively.
        let esc = name.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = writeln!(f, "{{\"span\":\"{esc}\",\"seconds\":{seconds:.6}}}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TeXRayLayer;
    use tracing::info_span;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    /// An examined span, entered under a TeXRay subscriber, lands one JSON line
    /// naming that span with a positive duration.
    #[test]
    fn records_examined_span_timing() {
        let path = std::env::temp_dir().join(format!("texray_sink_{}.jsonl", std::process::id()));
        let p = path.to_str().unwrap();
        to_file(p).unwrap();

        let subscriber = Registry::default().with(TeXRayLayer::new());
        tracing::subscriber::with_default(subscriber, || {
            crate::examine(info_span!("phase/demo")).in_scope(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
            });
        });

        let contents = std::fs::read_to_string(p).unwrap();
        *SINK.lock() = None; // release the file so other tests don't append
        let _ = std::fs::remove_file(p);
        assert!(
            contents.contains("\"span\":\"phase/demo\"") && contents.contains("\"seconds\":"),
            "sink did not record the span: {contents:?}",
        );
    }
}
