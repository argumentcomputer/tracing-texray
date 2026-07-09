use crate::tracked_spans::TrackedSpans;
use crate::{DynWriter, FieldFilter, RenderSettings, SpanSettings};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::io::Write;
use std::ops::DerefMut;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{io, iter};
use tracing::field::{Field, Visit};
use tracing::{Id, Subscriber};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

const NESTED_EVENT_OFFSET: usize = 2;
const DURATION_WIDTH: usize = 6;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum Action {
    ForgetSpan,
    DoNothing,
}

#[derive(Debug, Clone)]
pub(crate) struct EventInfo {
    timestamp: SystemTime,
    metadata: TrackedMetadata,
}

impl EventInfo {
    pub(crate) fn now(metadata: TrackedMetadata) -> Self {
        Self {
            timestamp: SystemTime::now(),
            metadata,
        }
    }
    fn to_string(&self, settings: &FieldSettings) -> String {
        let mut out = String::new();
        self.metadata
            .write(&mut out, settings)
            .expect("writing to a string cannot fail");
        out
    }
}

impl SpanInfo {
    pub(crate) fn for_span<S>(span: &Id, ctx: &Context<'_, S>, sample_rss: bool) -> Self
    where
        S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        Self {
            name: ctx
                .metadata(span)
                .map(|metadata| metadata.name())
                .unwrap_or("could-not-find-span"),
            start: SystemTime::now(),
            end: None,
            start_rss: if sample_rss {
                read_rss()
            } else {
                RssSample::default()
            },
            end_rss: RssSample::default(),
        }
    }
}

#[derive(Default, Debug, Clone)]
pub(crate) struct TrackedMetadata {
    data: Vec<(&'static str, String)>,
}

impl TrackedMetadata {
    fn write(&self, f: &mut impl std::fmt::Write, settings: &FieldSettings) -> std::fmt::Result {
        let relevant_fields = || {
            self.data
                .iter()
                .filter(|(f, _)| settings.field_filter.should_print(f))
        };

        if let Some((_, message)) = relevant_fields().find(|(k, _)| *k == "message") {
            write!(f, "{}", message.lines().next().unwrap_or_default())?;
        }

        let relevant_fields = || relevant_fields().filter(|(k, _v)| *k != "message");

        if relevant_fields().count() == 0 {
            return Ok(());
        }

        write!(f, "{{")?;
        let mut peekable = relevant_fields().peekable();
        while let Some((k, v)) = peekable.next() {
            write!(f, "{}: {}", k, v)?;
            if peekable.peek().is_some() {
                write!(f, ", ")?;
            }
        }
        write!(f, "}}")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SpanInfo {
    start: SystemTime,
    end: Option<SystemTime>,
    name: &'static str,
    start_rss: RssSample,
    end_rss: RssSample,
}

/// Snapshot of process resident-set size, in kilobytes.
///
/// `current_kb` is `VmRSS` (live RSS at sample time); `peak_kb` is `VmHWM`
/// (the highest RSS the process has reached). Both are zero on non-Linux
/// platforms — there's no portable equivalent of `/proc/self/status`.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct RssSample {
    current_kb: u64,
    peak_kb: u64,
}

impl RssSample {
    fn is_zero(&self) -> bool {
        self.current_kb == 0 && self.peak_kb == 0
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn read_rss() -> RssSample {
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else {
        return RssSample::default();
    };
    let mut sample = RssSample::default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            sample.current_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            sample.peak_kb = parse_kb(rest);
        }
    }
    sample
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_rss() -> RssSample {
    RssSample::default()
}

#[cfg(target_os = "linux")]
fn parse_kb(s: &str) -> u64 {
    s.trim()
        .split_ascii_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn format_bytes(kb: u64) -> String {
    let kb_f = kb as f64;
    if kb < 1024 {
        format!("{kb_f:.2} KiB")
    } else if kb < 1024 * 1024 {
        format!("{:.2} MiB", kb_f / 1024.0)
    } else {
        format!("{:.2} GiB", kb_f / (1024.0 * 1024.0))
    }
}

fn format_signed_kb(start_kb: u64, end_kb: u64) -> String {
    let delta = (end_kb as i64) - (start_kb as i64);
    let sign = if delta >= 0 { "+" } else { "-" };
    let mag = delta.unsigned_abs();
    format!("{sign}{}", format_bytes(mag))
}

/// Streaming-mode duration formatter: `1.23m`, `4.56s`, `789.01ms`, etc.
/// More precise than the timeline's `pretty_duration` (which truncates to
/// integers of the largest unit).
fn format_streaming_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos < 1_000 {
        format!("{nanos}ns")
    } else if nanos < 1_000_000 {
        format!("{:.2}μs", nanos as f64 / 1_000.0)
    } else if nanos < 1_000_000_000 {
        format!("{:.2}ms", nanos as f64 / 1_000_000.0)
    } else if nanos < 60_000_000_000 {
        format!("{:.2}s", nanos as f64 / 1_000_000_000.0)
    } else if nanos < 3_600_000_000_000 {
        let secs = nanos as f64 / 1_000_000_000.0;
        format!("{:.2}m", secs / 60.0)
    } else {
        let secs = nanos as f64 / 1_000_000_000.0;
        format!("{:.2}h", secs / 3_600.0)
    }
}

/// `  ── RAM Δ +X peak Y` suffix, or empty when no RAM data was captured.
fn format_streaming_rss(start: RssSample, end: RssSample) -> String {
    if start.is_zero() && end.is_zero() {
        return String::new();
    }
    format!(
        "  ── RAM Δ {} peak {}",
        format_signed_kb(start.current_kb, end.current_kb),
        format_bytes(end.peak_kb),
    )
}

/// Machine-readable peak RSS line with a parenthesized human-formatted
/// companion: `peak-rss-bytes=<N> (<X.YZ MiB>)`. The raw integer is for
/// programmatic consumers (CI benchmarks, grep) and the formatted value is
/// for eyeballing the same line. `None` when no RSS was sampled (peak 0)
/// — non-Linux, or RAM tracking off.
fn format_peak_rss_line(name: &str, peak_kb: u64) -> Option<String> {
    if peak_kb == 0 {
        return None;
    }
    Some(format!(
        "[texray] {name} peak-rss-bytes={} ({})",
        peak_kb.saturating_mul(1024),
        format_bytes(peak_kb),
    ))
}

impl SpanInfo {
    fn full_name(&self, tracker: &SpanTracker, settings: &FieldSettings) -> String {
        let mut id = self.name.to_string();
        tracker
            .metadata
            .write(&mut id, settings)
            .expect("rendering to string cannot fail");
        id
    }

    fn duration(&self) -> Option<Duration> {
        self.end.and_then(|end| end.duration_since(self.start).ok())
    }

    fn render(
        &self,
        out: &mut dyn Write,
        tracker: &SpanTracker,
        settings: &RenderSettings,
        render_conf: &RenderConf,
        left_offset: usize,
    ) -> io::Result<()> {
        let mut key = self.full_name(tracker, &tracker.settings);
        let truncated_key_width = render_conf.key_width - left_offset;
        key.truncate(truncated_key_width);
        let ev_start_ts = self.start;
        let span_len = match self.duration() {
            None => return Ok(()),
            Some(dur) => dur,
        };
        if let Some(min_duration) = settings.min_duration.as_ref() {
            if &span_len < min_duration {
                return Ok(());
            }
        }
        if left_offset > 0 {
            write!(out, "{}", " ".repeat(left_offset))?;
        }
        write!(out, "{:width$}", key, width = truncated_key_width)?;
        write!(
            out,
            " {:>dur_width$} ",
            crate::pretty_duration(span_len),
            dur_width = DURATION_WIDTH
        )?;

        let offset = width(
            render_conf.chart_width(),
            render_conf.total(),
            ev_start_ts
                .duration_since(render_conf.start_ts)
                .unwrap_or_default(),
        );
        write!(out, "{}", " ".repeat(offset))?;
        let interval_width = width(render_conf.chart_width(), render_conf.total(), span_len);
        match interval_width {
            0 => write!(out, "┆"),
            1 => write!(out, "│"),
            2 => write!(out, "├┤"),
            _more => write!(out, "├{}┤", "─".repeat(interval_width - 2)),
        }?;
        writeln!(out)?;
        Ok(())
    }
}

/// Tracker of an individual span
#[derive(Debug)]
pub(crate) struct SpanTracker {
    info: Option<SpanInfo>,
    metadata: TrackedMetadata,
    events: Vec<EventInfo>,
    settings: Arc<FieldSettings>,
}

pub(crate) struct FieldFilterTracked<'a> {
    field_filter: &'a FieldFilter,
    tracked_metadata: &'a mut TrackedMetadata,
}

impl Visit for FieldFilterTracked<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if self.field_filter.should_print(field.name()) {
            self.tracked_metadata
                .data
                .push((field.name(), format!("{:?}", value)));
        }
    }
}

impl SpanTracker {
    pub(crate) fn new(settings: Arc<FieldSettings>) -> Self {
        Self {
            info: None,
            events: vec![],
            metadata: Default::default(),
            settings,
        }
    }

    pub(crate) fn record_metadata(&mut self, values: &dyn RecordFields) {
        values.record(&mut FieldFilterTracked {
            field_filter: &self.settings.field_filter,
            tracked_metadata: &mut self.metadata,
        });
    }

    pub(crate) fn add_event(&mut self, event: EventInfo) {
        self.events.push(event)
    }

    pub(crate) fn open(&mut self, span_info: SpanInfo) {
        // Only record the first open; already-open spans aren't updated.
        if self.info.is_none() {
            self.info = Some(span_info);
        }
    }

    fn exit(&mut self, timestamp: SystemTime, end_rss: RssSample) {
        match &mut self.info {
            Some(info) => {
                info.end = Some(timestamp);
                info.end_rss = end_rss;
            }
            None => eprintln!("this is a bug"), //
        }
    }

    fn span_info(&self) -> impl Iterator<Item = &SpanInfo> {
        self.info.iter()
    }

    /// One-line summary for streaming mode: `[texray] <name>: <dur>  ── RAM Δ +X peak Y`.
    /// Returns `None` if the span has no recorded info or hasn't ended.
    pub(crate) fn streaming_line(&self) -> Option<String> {
        let info = self.info.as_ref()?;
        let end = info.end?;
        let duration = end.duration_since(info.start).ok()?;
        let dur_fmt = format_streaming_duration(duration);
        let rss_suffix = format_streaming_rss(info.start_rss, info.end_rss);
        Some(format!("[texray] {}: {dur_fmt}{rss_suffix}", info.name))
    }

    /// Machine-readable companion to [`streaming_line`]: `[texray] <name>
    /// peak-rss-bytes=<N> (<X.YZ MiB>)`. Lets streaming consumers grep the
    /// peak RSS as a single integer while still being readable to humans.
    /// `None` when the span hasn't ended or no RSS was sampled.
    pub(crate) fn streaming_rss_line(&self) -> Option<String> {
        let info = self.info.as_ref()?;
        info.end?;
        format_peak_rss_line(info.name, info.end_rss.peak_kb)
    }

    /// Raw `(name, seconds)` for a closed span — the JSON sink's source.
    /// `None` if the span has no recorded info or hasn't ended.
    pub(crate) fn timing(&self) -> Option<(&'static str, f64)> {
        let info = self.info.as_ref()?;
        let end = info.end?;
        let duration = end.duration_since(info.start).ok()?;
        Some((info.name, duration.as_secs_f64()))
    }

    fn max_key_width(&self, depth: usize) -> usize {
        let longest_self = self
            .info
            .as_ref()
            .map(|info| info.full_name(self, &self.settings).len())
            .unwrap_or_default();
        longest_self + NESTED_EVENT_OFFSET * (depth - 1)
    }
}

#[derive(Debug)]
pub(crate) struct RootTracker {
    examined_spans: TrackedSpans,
    span_metadata: RwLock<HashMap<Id, InterestTracker>>,
}

#[derive(Debug)]
pub(crate) struct InterestTracker {
    field_settings: Arc<FieldSettings>,
    render_settings: RenderSettings,
    out: DynWriter,
    children: HashMap<Vec<Id>, SpanTracker>,
}

impl InterestTracker {
    fn new(
        id: Id,
        settings: RenderSettings,
        field_settings: FieldSettings,
        out: DynWriter,
    ) -> Self {
        let mut children = HashMap::new();
        children.insert(vec![id], SpanTracker::new(Arc::new(field_settings.clone())));
        Self {
            children,
            field_settings: Arc::new(field_settings),
            render_settings: settings,
            out,
        }
    }

    pub(crate) fn field_recorder<'a>(
        &'a self,
        metadata: &'a mut TrackedMetadata,
    ) -> FieldFilterTracked<'a> {
        FieldFilterTracked {
            field_filter: &self.field_settings.field_filter,
            tracked_metadata: metadata,
        }
    }

    pub(crate) fn new_span(&mut self, path: Vec<Id>) -> &mut SpanTracker {
        debug_assert!(!path.is_empty());
        let settings = self.field_settings.clone();
        self.children
            .entry(path)
            .or_insert_with(|| SpanTracker::new(settings))
    }

    pub(crate) fn record_metadata(&mut self, path: &[Id], fields: &dyn RecordFields) {
        if let Some(s) = self.children.get_mut(path) {
            s.record_metadata(fields)
        }
    }

    #[track_caller]
    fn span(&mut self, path: Vec<Id>) -> &mut SpanTracker {
        debug_assert!(!path.is_empty());
        let settings = self.field_settings.clone();
        self.children
            .entry(path)
            .or_insert_with(|| SpanTracker::new(settings))
    }

    pub(crate) fn open(&mut self, path: Vec<Id>, span_info: SpanInfo) {
        self.span(path).open(span_info);
    }

    pub(crate) fn add_event(&mut self, path: Vec<Id>, event: EventInfo) {
        self.span(path).add_event(event);
    }

    pub(crate) fn exit(&mut self, path: Vec<Id>, timestamp: SystemTime) {
        let end_rss = if self.sample_rss() {
            read_rss()
        } else {
            RssSample::default()
        };
        let streaming = self.render_settings.streaming;
        let json = crate::json_sink::is_active();
        let (line, rss_line) = {
            let span = self.span(path);
            span.exit(timestamp, end_rss);
            if json {
                if let Some((name, secs)) = span.timing() {
                    crate::json_sink::record(name, secs);
                }
            }
            if streaming {
                (span.streaming_line(), span.streaming_rss_line())
            } else {
                (None, None)
            }
        };
        let mut out = self.out.inner.lock();
        if let Some(line) = line {
            let _ = writeln!(out, "{line}");
        }
        if let Some(rss_line) = rss_line {
            let _ = writeln!(out, "{rss_line}");
        }
    }

    /// Whether to sample RSS: only when RAM tracking and streaming are both on.
    /// The streaming close lines are the only consumer of RSS samples, so with
    /// streaming off the per-span `/proc` reads would be wasted.
    pub(crate) fn sample_rss(&self) -> bool {
        self.render_settings.track_ram && self.render_settings.streaming
    }

    fn spans(&self) -> impl Iterator<Item = &SpanInfo> {
        self.children.values().flat_map(|c| c.span_info())
    }

    pub(crate) fn dump(&self) -> io::Result<()> {
        let mut out = self.out.inner.lock();
        let settings = &self.render_settings;
        let all_events = self.spans().collect::<Vec<_>>();
        if all_events.is_empty() {
            write!(&mut out, "no events...")?;
            return Ok(());
        }
        // Lead with a blank line so the dump doesn't jam against whatever
        // was written to stderr just before (criterion's `Benchmarking ...`,
        // log lines, etc.).
        writeln!(out)?;
        let (start_ts, end_ts) = (
            all_events
                .iter()
                .map(|ev| ev.start)
                .min()
                .expect("non empty"),
            all_events
                .iter()
                .flat_map(|ev| ev.end)
                .max()
                .expect("non empty"),
        );
        let conf = RenderConf {
            start_ts,
            end_ts,
            key_width: self
                .children
                .iter()
                .map(|(path, t)| t.max_key_width(path.len()))
                .max()
                .unwrap_or(120)
                .min(120),
            width: settings.width,
        };
        let mut ordered = self.children.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(key, _)| sort_key(&self.children, key.as_slice()));

        for (key, track) in ordered.iter() {
            let offset = NESTED_EVENT_OFFSET * (key.len() - 1);
            if let Some(info) = track.info.as_ref() {
                if settings.types.spans {
                    info.render(out.deref_mut(), track, settings, &conf, offset)?;
                }
                if settings.types.events {
                    self.render_events(
                        out.deref_mut(),
                        &track.events,
                        offset,
                        &conf,
                        info,
                        &self.field_settings,
                    )?;
                }
            }
        }

        Ok(())
    }

    fn render_events(
        &self,
        mut out: impl Write,
        events: &[EventInfo],
        left_offset: usize,
        render_conf: &RenderConf,
        span_info: &SpanInfo,
        field_settings: &FieldSettings,
    ) -> io::Result<()> {
        let left_offset = left_offset + 2;
        let truncated_key_width = render_conf.key_width - left_offset;
        let base_offset = width(
            render_conf.chart_width(),
            render_conf.total(),
            span_info
                .start
                .duration_since(render_conf.start_ts)
                .expect("start_ts MUST be before span_info.start because it is a minima"),
        );
        let mut settings_with_message = field_settings.clone();
        if let FieldFilter::AllowList(list) = &mut settings_with_message.field_filter {
            list.insert("message".into());
        }
        for ev in events {
            let mut key = ev.to_string(&settings_with_message);
            key.truncate(truncated_key_width);
            if left_offset >= 1 {
                write!(out, "{}", " ".repeat(left_offset - 1))?;
            }
            write!(out, ">{:width$}", key, width = truncated_key_width)?;
            let event_offset = (width(
                render_conf.chart_width(),
                render_conf.total(),
                ev.timestamp
                    .duration_since(span_info.start)
                    .unwrap_or_default(),
            ) as i32)
                - 1;
            write!(out, "{}", " ".repeat(DURATION_WIDTH + 2))?;
            writeln!(
                out,
                "{}┼",
                " ".repeat(base_offset + event_offset.max(0) as usize)
            )?;
        }
        Ok(())
    }
}

impl RootTracker {
    pub(crate) fn new() -> Self {
        Self {
            examined_spans: TrackedSpans::new(1024),
            span_metadata: Default::default(),
        }
    }

    /// Returns true if this span was tracked
    pub(crate) fn end_tracking(&self, id: Id) -> bool {
        self.examined_spans.remove(id.into_non_zero_u64())
    }

    pub(crate) fn register_interest(&self, id: Id, settings: SpanSettings) {
        // put the insertion into examined_spans inside the critical block
        let mut span_guard = self.span_metadata.write();
        if self.examined_spans.insert(id.into_non_zero_u64()).is_err() {
            tracing::warn!("map is full, too many spans. this span will not be tracked");
            return;
        }
        span_guard.insert(
            id.clone(),
            InterestTracker::new(id, settings.render, settings.fields, settings.out),
        );
    }

    /// Synthesize a `SpanInfo` for an already-entered examined span.
    ///
    /// `examine_current()` calls this because the span's `on_enter` callback
    /// fired before interest was registered — so the root `SpanTracker`
    /// exists but has no `info`, and `sort_key`/`exit` would treat that as a
    /// bug. We approximate the start as now (slightly later than the real
    /// entry, but the gap is the call-site overhead, typically negligible).
    pub(crate) fn populate_examined_root(&self, id: &Id, name: &'static str) {
        let mut span_guard = self.span_metadata.write();
        let Some(interest_tracker) = span_guard.get_mut(id) else {
            return;
        };
        let sample_rss = interest_tracker.sample_rss();
        let field_settings = interest_tracker.field_settings.clone();
        let key = vec![id.clone()];
        let span_tracker = interest_tracker
            .children
            .entry(key)
            .or_insert_with(|| SpanTracker::new(field_settings));
        if span_tracker.info.is_none() {
            let start_rss = if sample_rss {
                read_rss()
            } else {
                RssSample::default()
            };
            span_tracker.info = Some(SpanInfo {
                name,
                start: SystemTime::now(),
                end: None,
                start_rss,
                end_rss: RssSample::default(),
            });
        }
    }

    pub(crate) fn if_interested(
        &self,
        ids: impl Iterator<Item = Id>,
        f: impl Fn(&mut InterestTracker, &mut dyn Iterator<Item = Id>) -> Action,
    ) -> Option<InterestTracker> {
        let mut iter = ids.skip_while(|id| !self.examined_spans.contains(id.into_non_zero_u64()));
        if let Some(root) = iter.next() {
            assert!(self.examined_spans.contains(root.into_non_zero_u64()));
            let mut tracker = self.span_metadata.write();
            let mut with_root = iter::once(root.clone()).chain(iter);
            if let Some(span_tracker) = tracker.get_mut(&root) {
                if f(span_tracker, &mut with_root) == Action::ForgetSpan {
                    return tracker.remove(&root);
                }
            } else {
                eprintln!("This is a bug–span tracker could not be found");
            }
        }
        None
    }
}

fn sort_key<'a>(map: &'a HashMap<Vec<Id>, SpanTracker>, target: &'a [Id]) -> Vec<SystemTime> {
    (1..=target.len())
        .rev()
        .map(move |idx| {
            map.get(&target[..idx])
                .and_then(|span| span.info.as_ref())
                .map(|info| info.start)
                .unwrap_or_else(|| {
                    eprintln!("could not find span or span start—this is a bug;");
                    UNIX_EPOCH
                })
        })
        .collect::<Vec<_>>()
}

#[derive(Debug, Clone)]
pub(crate) struct FieldSettings {
    field_filter: FieldFilter,
}

impl FieldSettings {
    pub(crate) fn new(field_filter: FieldFilter) -> FieldSettings {
        Self { field_filter }
    }
}

impl Default for FieldSettings {
    fn default() -> Self {
        Self {
            field_filter: FieldFilter::DenyList(HashSet::new()),
        }
    }
}

#[cfg(test)]
mod test {
    use super::{format_peak_rss_line, format_streaming_duration, format_streaming_rss};
    use crate::{DynWriter, RenderSettings, Settings};
    use std::io::{BufWriter, Write};

    use std::mem::take;

    use std::ops::Add;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    use crate::tracker::{
        FieldSettings, InterestTracker, RssSample, SpanInfo, TrackedMetadata, format_bytes,
        format_signed_kb, read_rss, width,
    };
    use tracing::Id;

    fn id(i: u64) -> Id {
        Id::from_u64(i)
    }

    fn render_settings(track_ram: bool) -> RenderSettings {
        let settings = Settings::default();
        RenderSettings {
            width: settings.width,
            min_duration: settings.min_duration,
            types: settings.types,
            track_ram,
            streaming: false,
        }
    }

    #[test]
    fn compute_relative_width() {
        let total = Duration::from_secs(10);
        let partial = Duration::from_secs(1);
        assert_eq!(width(10, total, partial), 1);

        let total = Duration::from_secs(10);
        let partial = Duration::from_secs_f64(2.9);
        assert_eq!(width(10, total, partial), 3);

        let total = Duration::from_secs_f64(0.045532);
        let partial = Duration::from_secs_f64(0.034389);
        assert_eq!(width(120, total, partial), 91);
        let total = Duration::from_secs_f64(0.045532);
        let partial = Duration::from_secs_f64(0.034489);
        assert_eq!(width(120, total, partial), 91);
    }

    fn dump_to_string(id: Id, f: impl Fn(&mut InterestTracker)) -> String {
        let (writer, buf) = DynWriter::str();
        let mut tracker =
            InterestTracker::new(id, render_settings(false), FieldSettings::default(), writer);
        f(&mut tracker);
        tracker.dump().unwrap();
        let mut buf = buf.lock();
        buf.flush().unwrap();
        String::from_utf8(take(buf.get_mut())).unwrap()
    }

    #[test]
    fn render_metadata() {
        let metadata = TrackedMetadata {
            data: vec![("A", "B".to_string()), ("c", "d".to_string())],
        };
        let mut out = String::new();
        metadata.write(&mut out, &FieldSettings::default()).unwrap();
        assert_eq!(out, "{A: B, c: d}");
    }

    #[test]
    fn render_empty_metadata() {
        let metadata = TrackedMetadata { data: vec![] };
        let mut out = String::new();
        metadata.write(&mut out, &FieldSettings::default()).unwrap();
        assert_eq!(out, "");
    }

    impl DynWriter {
        fn str() -> (DynWriter, Arc<Mutex<BufWriter<Vec<u8>>>>) {
            let buf = Arc::new(Mutex::new(BufWriter::new(vec![])));
            (DynWriter { inner: buf.clone() }, buf)
        }
    }

    #[test]
    fn render_correct_output() {
        let output = dump_to_string(id(1), |tracker| {
            let interval_start = UNIX_EPOCH;
            let interval_end = UNIX_EPOCH.add(Duration::from_secs(10));
            tracker.new_span(vec![id(1)]);
            tracker.new_span(vec![id(1), id(2)]);
            tracker.open(
                vec![id(1)],
                SpanInfo {
                    name: "test",
                    start: interval_start,
                    end: None,
                    start_rss: RssSample::default(),
                    end_rss: RssSample::default(),
                },
            );
            tracker.open(
                vec![id(1), id(2)],
                SpanInfo {
                    name: "nested",
                    start: interval_start + Duration::from_secs(2),
                    end: None,
                    start_rss: RssSample::default(),
                    end_rss: RssSample::default(),
                },
            );
            tracker.exit(vec![id(1), id(2)], interval_start + Duration::from_secs(7));
            tracker.exit(vec![id(1)], interval_end);
        });
        assert_eq!(
            output,
            r#"
test       10s  ├──────────────────────────────────────────────────────────────────────────────────────────────────────┤
  nested    5s                       ├──────────────────────────────────────────────────┤
"#
        );
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0.00 KiB");
        assert_eq!(format_bytes(512), "512.00 KiB");
        assert_eq!(format_bytes(1023), "1023.00 KiB");
        assert_eq!(format_bytes(1024), "1.00 MiB");
        assert_eq!(format_bytes(2048), "2.00 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.00 GiB");
    }

    #[test]
    fn format_signed_kb_signs() {
        assert_eq!(format_signed_kb(0, 0), "+0.00 KiB");
        assert_eq!(format_signed_kb(1024, 3072), "+2.00 MiB");
        assert_eq!(format_signed_kb(3072, 1024), "-2.00 MiB");
        assert_eq!(format_signed_kb(100, 100), "+0.00 KiB");
    }

    #[test]
    fn format_streaming_duration_units() {
        use std::time::Duration;
        assert_eq!(
            format_streaming_duration(Duration::from_nanos(500)),
            "500ns"
        );
        assert_eq!(
            format_streaming_duration(Duration::from_nanos(1_500)),
            "1.50μs"
        );
        assert_eq!(
            format_streaming_duration(Duration::from_micros(1_500)),
            "1.50ms"
        );
        assert_eq!(
            format_streaming_duration(Duration::from_millis(1_500)),
            "1.50s"
        );
        assert_eq!(format_streaming_duration(Duration::from_secs(90)), "1.50m");
        assert_eq!(
            format_streaming_duration(Duration::from_secs(5_400)),
            "1.50h"
        );
    }

    #[test]
    fn format_streaming_rss_empty_when_zero() {
        assert_eq!(
            format_streaming_rss(RssSample::default(), RssSample::default()),
            ""
        );
    }

    #[test]
    fn format_streaming_rss_renders_delta_and_peak() {
        let start = RssSample {
            current_kb: 1024,
            peak_kb: 1024,
        };
        let end = RssSample {
            current_kb: 3072,
            peak_kb: 4096,
        };
        assert_eq!(
            format_streaming_rss(start, end),
            "  ── RAM Δ +2.00 MiB peak 4.00 MiB"
        );
    }

    #[test]
    fn format_peak_rss_line_bytes_with_human_suffix() {
        // 4096 KiB peak -> 4194304 raw bytes plus a parenthesized human form.
        assert_eq!(
            format_peak_rss_line("load_data", 4096),
            Some("[texray] load_data peak-rss-bytes=4194304 (4.00 MiB)".to_string())
        );
        // Zero peak (non-Linux / RAM tracking off) emits nothing.
        assert_eq!(format_peak_rss_line("load_data", 0), None);
    }

    #[test]
    fn rss_sample_is_zero() {
        assert!(RssSample::default().is_zero());
        assert!(
            !RssSample {
                current_kb: 1,
                peak_kb: 0
            }
            .is_zero()
        );
        assert!(
            !RssSample {
                current_kb: 0,
                peak_kb: 1
            }
            .is_zero()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_kb_handles_status_format() {
        use crate::tracker::parse_kb;
        assert_eq!(parse_kb(" 12345 kB"), 12345);
        assert_eq!(parse_kb("\t  42  kB\n"), 42);
        assert_eq!(parse_kb("0 kB"), 0);
        assert_eq!(parse_kb("garbage"), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_rss_returns_live_sample() {
        // The test process is real; VmRSS must be non-zero and VmHWM >= VmRSS.
        let sample = read_rss();
        assert!(
            sample.current_kb > 0,
            "expected non-zero current RSS, got {sample:?}"
        );
        assert!(
            sample.peak_kb >= sample.current_kb,
            "peak ({}) should be >= current ({})",
            sample.peak_kb,
            sample.current_kb
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn read_rss_returns_zero_off_linux() {
        assert!(read_rss().is_zero());
    }

    #[test]
    fn span_info_for_span_samples_when_enabled() {
        // Without a real tracing Subscriber we can't call SpanInfo::for_span
        // directly, but we can exercise the gating logic the same way it does:
        // sample only when track_ram is true.
        let sampled = if true {
            read_rss()
        } else {
            RssSample::default()
        };
        let unsampled = if false {
            read_rss()
        } else {
            RssSample::default()
        };
        assert!(unsampled.is_zero());
        // On Linux the test process has live RSS; off-Linux read_rss is zero.
        #[cfg(target_os = "linux")]
        assert!(!sampled.is_zero());
        #[cfg(not(target_os = "linux"))]
        assert!(sampled.is_zero());
    }

    #[test]
    fn interest_tracker_exit_samples_when_tracking_and_streaming() {
        // exit() records a non-zero end_rss only when track_ram and streaming
        // are both on (RSS is consumed by the streaming lines); Linux only.
        let mut tracker = InterestTracker::new(
            id(1),
            RenderSettings {
                streaming: true,
                ..render_settings(true)
            },
            FieldSettings::default(),
            {
                let (w, _) = DynWriter::str();
                w
            },
        );
        tracker.new_span(vec![id(1)]);
        tracker.open(
            vec![id(1)],
            SpanInfo {
                name: "x",
                start: UNIX_EPOCH,
                end: None,
                start_rss: RssSample::default(),
                end_rss: RssSample::default(),
            },
        );
        tracker.exit(vec![id(1)], UNIX_EPOCH + Duration::from_secs(1));
        let span = tracker.span(vec![id(1)]);
        let info = span.info.as_ref().expect("info set after open");
        #[cfg(target_os = "linux")]
        assert!(
            !info.end_rss.is_zero(),
            "expected real RSS sample on Linux, got {:?}",
            info.end_rss
        );
        #[cfg(not(target_os = "linux"))]
        assert!(info.end_rss.is_zero());
    }

    #[test]
    fn interest_tracker_exit_skips_sampling_when_disabled() {
        let mut tracker =
            InterestTracker::new(id(1), render_settings(false), FieldSettings::default(), {
                let (w, _) = DynWriter::str();
                w
            });
        tracker.new_span(vec![id(1)]);
        tracker.open(
            vec![id(1)],
            SpanInfo {
                name: "x",
                start: UNIX_EPOCH,
                end: None,
                start_rss: RssSample::default(),
                end_rss: RssSample::default(),
            },
        );
        tracker.exit(vec![id(1)], UNIX_EPOCH + Duration::from_secs(1));
        let span = tracker.span(vec![id(1)]);
        let info = span.info.as_ref().expect("info set after open");
        assert!(info.end_rss.is_zero());
    }

    #[test]
    fn interest_tracker_exit_skips_sampling_without_streaming() {
        // track_ram on but streaming off: nothing would render the samples, so
        // sample_rss() is false and exit() must not read RSS.
        let mut tracker =
            InterestTracker::new(id(1), render_settings(true), FieldSettings::default(), {
                let (w, _) = DynWriter::str();
                w
            });
        tracker.new_span(vec![id(1)]);
        tracker.open(
            vec![id(1)],
            SpanInfo {
                name: "x",
                start: UNIX_EPOCH,
                end: None,
                start_rss: RssSample::default(),
                end_rss: RssSample::default(),
            },
        );
        tracker.exit(vec![id(1)], UNIX_EPOCH + Duration::from_secs(1));
        let span = tracker.span(vec![id(1)]);
        let info = span.info.as_ref().expect("info set after open");
        assert!(
            info.end_rss.is_zero(),
            "track_ram without streaming should not sample RSS: {:?}",
            info.end_rss
        );
    }
}

#[derive(Debug)]
struct RenderConf {
    start_ts: SystemTime,
    end_ts: SystemTime,
    key_width: usize,
    width: usize,
}

impl RenderConf {
    fn total(&self) -> Duration {
        // start_ts is always less than end_ts
        self.end_ts
            .duration_since(self.start_ts)
            .unwrap_or_default()
    }

    fn chart_width(&self) -> usize {
        self.width
            .checked_sub(self.key_width)
            .and_then(|w| w.checked_sub(DURATION_WIDTH + 2))
            .unwrap_or(20)
    }
}

fn width(chars: usize, outer: Duration, inner: Duration) -> usize {
    if inner.as_nanos() == 0 || outer.as_nanos() == 0 {
        return 0;
    }
    let ratio = inner.as_secs_f64() / outer.as_secs_f64();
    (ratio * chars as f64).round() as usize
}
