use parking_lot::Mutex;
use std::fmt::{Display, Formatter};
use std::io::Write;
use std::sync::Arc;
use tracing::info_span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_texray::TeXRayLayer;

#[tracing::instrument(name = "instrumented_outer", skip_all)]
fn instrumented_work() {
    tracing_texray::examine_current();
    info_span!("inner").in_scope(|| {
        std::thread::sleep(std::time::Duration::from_millis(1));
    });
}

#[test]
fn examine_current_dumps_instrumented_span() {
    let writer = CaptureWriter::new();
    let layer = TeXRayLayer::new()
        .width(80)
        .update_settings(|s| s.writer(writer.clone()));
    let registry = tracing_subscriber::registry().with(layer);
    tracing::subscriber::set_global_default(registry)
        .expect("failed to install subscriber");

    instrumented_work();

    let output = writer.to_string();
    assert!(
        output.contains("instrumented_outer"),
        "expected outer span name in dump:\n{output}"
    );
    assert!(
        output.contains("inner"),
        "expected nested span in dump:\n{output}"
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
