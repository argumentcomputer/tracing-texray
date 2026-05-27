use parking_lot::Mutex;
use std::fmt::{Display, Formatter};
use std::io::Write;
use std::sync::Arc;
use tracing::info_span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_texray::TeXRayLayer;

#[tracing::instrument(name = "outer", skip_all)]
fn instrumented_work() {
    tracing_texray::examine_current();
    info_span!("inner").in_scope(|| {
        std::thread::sleep(std::time::Duration::from_millis(1));
    });
}

#[test]
fn streaming_emits_line_per_span_close() {
    let writer = CaptureWriter::new();
    let layer = TeXRayLayer::new()
        .width(80)
        .streaming()
        .track_ram()
        .update_settings(|s| s.writer(writer.clone()));
    let registry = tracing_subscriber::registry().with(layer);
    tracing::subscriber::set_global_default(registry).expect("failed to install subscriber");

    instrumented_work();

    let output = writer.to_string();
    // Streaming lines should appear for each tracked span as it closes.
    assert!(
        output.contains("[texray] outer:"),
        "expected outer span streaming line:\n{output}"
    );
    assert!(
        output.contains("[texray] inner:"),
        "expected inner span streaming line:\n{output}"
    );
    // Streaming lines should fire *before* the texray graph prints — the
    // inner span closes first, then outer (which also triggers the print).
    let inner_pos = output
        .find("[texray] inner:")
        .expect("inner streaming line missing");
    let outer_pos = output
        .find("[texray] outer:")
        .expect("outer streaming line missing");
    assert!(
        inner_pos < outer_pos,
        "expected inner streaming line before outer:\n{output}"
    );
    // track_ram() is enabled; on Linux each streaming line should carry a
    // `── RAM Δ ... peak ...` suffix from real RSS sampling. Off-Linux the
    // sample is zero and the suffix is suppressed.
    #[cfg(target_os = "linux")]
    assert!(
        output.contains("── RAM Δ"),
        "expected RAM suffix on streaming lines with track_ram on Linux:\n{output}"
    );
}

#[derive(Clone)]
struct CaptureWriter {
    data: Arc<Mutex<Vec<u8>>>,
}

impl CaptureWriter {
    fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Display for CaptureWriter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.data.lock()))
    }
}

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
