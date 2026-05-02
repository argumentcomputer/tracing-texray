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
    pub(crate) fn for_span<S>(span: &Id, ctx: &Context<'_, S>, track_ram: bool) -> Self
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
            start_rss: if track_ram { read_rss() } else { RssSample::default() },
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
        match self.info {
            None => self.info = Some(span_info),
            Some(_) => {} // already open, don't update
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
        let end_rss = if self.render_settings.track_ram {
            read_rss()
        } else {
            RssSample::default()
        };
        self.span(path).exit(timestamp, end_rss);
    }

    pub(crate) fn track_ram(&self) -> bool {
        self.render_settings.track_ram
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

        if settings.track_ram {
            self.render_ram(out.deref_mut(), &ordered)?;
        }

        Ok(())
    }

    fn render_ram(
        &self,
        mut out: impl Write,
        ordered: &[(&Vec<Id>, &SpanTracker)],
    ) -> io::Result<()> {
        let any_rss = ordered.iter().any(|(_, track)| {
            track
                .info
                .as_ref()
                .map(|info| !info.start_rss.is_zero() || !info.end_rss.is_zero())
                .unwrap_or(false)
        });
        if !any_rss {
            return Ok(());
        }

        let mut rows: Vec<(String, String, String)> = Vec::with_capacity(ordered.len());
        let mut max_label = 0;
        let mut max_delta = 0;
        for (key, track) in ordered {
            let Some(info) = track.info.as_ref() else {
                continue;
            };
            // Mirror the bar chart's `min_duration` filter so the RAM block
            // doesn't list rows that aren't shown in the timeline above.
            if let Some(min) = self.render_settings.min_duration.as_ref() {
                let span_len = match info.duration() {
                    None => continue,
                    Some(d) => d,
                };
                if &span_len < min {
                    continue;
                }
            }
            let depth = key.len().saturating_sub(1);
            let label = format!(
                "{}{}",
                " ".repeat(NESTED_EVENT_OFFSET * depth),
                info.full_name(track, &self.field_settings),
            );
            // Show start→end RSS (the trajectory) so transient allocations
            // are visible: a span whose end is well below its peak freed
            // memory before exiting. Append the net delta in parens so it's
            // also legible at a glance.
            let delta = format!(
                "RSS {} → {} (Δ {})",
                format_bytes(info.start_rss.current_kb),
                format_bytes(info.end_rss.current_kb),
                format_signed_kb(info.start_rss.current_kb, info.end_rss.current_kb),
            );
            let peak = format!("peak {}", format_bytes(info.end_rss.peak_kb));
            max_label = max_label.max(label.chars().count());
            max_delta = max_delta.max(delta.chars().count());
            rows.push((label, delta, peak));
        }

        writeln!(out)?;
        writeln!(out, "RAM:")?;
        for (label, delta, peak) in rows {
            let label_pad = max_label - label.chars().count();
            let delta_pad = max_delta - delta.chars().count();
            writeln!(
                out,
                "  {label}{}  {delta}{}  {peak}",
                " ".repeat(label_pad),
                " ".repeat(delta_pad),
            )?;
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
        let track_ram = interest_tracker.render_settings.track_ram;
        let field_settings = interest_tracker.field_settings.clone();
        let key = vec![id.clone()];
        let span_tracker = interest_tracker
            .children
            .entry(key)
            .or_insert_with(|| SpanTracker::new(field_settings));
        if span_tracker.info.is_none() {
            let start_rss = if track_ram {
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
    use crate::{DynWriter, RenderSettings, Settings};
    use std::io::{BufWriter, Write};

    use std::mem::take;

    use std::ops::Add;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::tracker::{
        format_bytes, format_signed_kb, read_rss, width, FieldSettings, InterestTracker,
        RssSample, SpanInfo, TrackedMetadata,
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
        dump_to_string_with(id, render_settings(false), f)
    }

    fn dump_to_string_with(
        id: Id,
        settings: RenderSettings,
        f: impl Fn(&mut InterestTracker),
    ) -> String {
        let (writer, buf) = DynWriter::str();
        let mut tracker = InterestTracker::new(id, settings, FieldSettings::default(), writer);
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
    fn rss_sample_is_zero() {
        assert!(RssSample::default().is_zero());
        assert!(!RssSample {
            current_kb: 1,
            peak_kb: 0
        }
        .is_zero());
        assert!(!RssSample {
            current_kb: 0,
            peak_kb: 1
        }
        .is_zero());
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

    /// Build a SpanInfo with explicit RSS samples and a closed time range,
    /// bypassing the live `read_rss()` call so render output is deterministic.
    fn closed_span(
        name: &'static str,
        start: SystemTime,
        end: SystemTime,
        start_rss: RssSample,
        end_rss: RssSample,
    ) -> SpanInfo {
        SpanInfo {
            name,
            start,
            end: Some(end),
            start_rss,
            end_rss,
        }
    }

    #[test]
    fn render_ram_block_present_with_samples() {
        let interval_start = UNIX_EPOCH;
        let output = dump_to_string_with(id(1), render_settings(true), |tracker| {
            tracker.new_span(vec![id(1)]);
            tracker.new_span(vec![id(1), id(2)]);
            tracker.open(
                vec![id(1)],
                closed_span(
                    "outer",
                    interval_start,
                    interval_start + Duration::from_secs(10),
                    RssSample {
                        current_kb: 1024,
                        peak_kb: 1024,
                    },
                    RssSample {
                        current_kb: 3072,
                        peak_kb: 4096,
                    },
                ),
            );
            tracker.open(
                vec![id(1), id(2)],
                closed_span(
                    "inner",
                    interval_start + Duration::from_secs(2),
                    interval_start + Duration::from_secs(7),
                    RssSample {
                        current_kb: 2048,
                        peak_kb: 2048,
                    },
                    RssSample {
                        current_kb: 1024,
                        peak_kb: 4096,
                    },
                ),
            );
        });

        assert!(output.contains("RAM:"), "missing RAM header in:\n{output}");
        assert!(
            output.contains("  outer    RSS 1.00 MiB → 3.00 MiB (Δ +2.00 MiB)  peak 4.00 MiB"),
            "missing outer row in:\n{output}"
        );
        assert!(
            output.contains("    inner  RSS 2.00 MiB → 1.00 MiB (Δ -1.00 MiB)  peak 4.00 MiB"),
            "missing inner row in:\n{output}"
        );
        // RAM block must come after the bar chart (parent first, child indented).
        let bar_pos = output.find("outer").expect("bar row");
        let ram_pos = output.find("RAM:").expect("ram header");
        assert!(bar_pos < ram_pos, "RAM block must follow bars:\n{output}");
        assert!(
            output.find("outer").map(|p| p < ram_pos).unwrap_or(false)
                && output.rfind("outer").map(|p| p > ram_pos).unwrap_or(false),
            "outer should appear in both bar and RAM sections:\n{output}"
        );
    }

    #[test]
    fn render_ram_block_respects_min_duration() {
        // `min_duration` should filter both the bar chart and the RAM block
        // so they stay consistent. A short span should be hidden in both.
        let mut settings = render_settings(true);
        settings.min_duration = Some(Duration::from_millis(5));
        let interval_start = UNIX_EPOCH;
        let output = dump_to_string_with(id(1), settings, |tracker| {
            tracker.new_span(vec![id(1)]);
            tracker.new_span(vec![id(1), id(2)]);
            // Long parent: kept.
            tracker.open(
                vec![id(1)],
                closed_span(
                    "outer",
                    interval_start,
                    interval_start + Duration::from_millis(100),
                    RssSample {
                        current_kb: 1024,
                        peak_kb: 1024,
                    },
                    RssSample {
                        current_kb: 3072,
                        peak_kb: 4096,
                    },
                ),
            );
            // Sub-millisecond child: filtered.
            tracker.open(
                vec![id(1), id(2)],
                closed_span(
                    "inner_short",
                    interval_start + Duration::from_millis(10),
                    interval_start + Duration::from_millis(11),
                    RssSample {
                        current_kb: 2048,
                        peak_kb: 2048,
                    },
                    RssSample {
                        current_kb: 1024,
                        peak_kb: 4096,
                    },
                ),
            );
        });

        assert!(output.contains("RAM:"), "missing RAM header in:\n{output}");
        let ram_pos = output.find("RAM:").expect("ram header");
        let ram_block = &output[ram_pos..];
        assert!(
            ram_block.contains("outer"),
            "outer (100ms) should appear in RAM block:\n{output}"
        );
        assert!(
            !ram_block.contains("inner_short"),
            "inner_short (1ms) should be filtered by min_duration=5ms in RAM block:\n{output}"
        );
    }

    #[test]
    fn render_ram_block_skipped_when_disabled() {
        let interval_start = UNIX_EPOCH;
        let output = dump_to_string_with(id(1), render_settings(false), |tracker| {
            tracker.new_span(vec![id(1)]);
            tracker.open(
                vec![id(1)],
                closed_span(
                    "outer",
                    interval_start,
                    interval_start + Duration::from_secs(1),
                    RssSample {
                        current_kb: 1024,
                        peak_kb: 1024,
                    },
                    RssSample {
                        current_kb: 2048,
                        peak_kb: 2048,
                    },
                ),
            );
        });
        assert!(
            !output.contains("RAM:"),
            "RAM block should be absent when track_ram=false:\n{output}"
        );
    }

    #[test]
    fn render_ram_block_skipped_when_all_samples_zero() {
        // track_ram is on but no real samples were taken (e.g. non-Linux);
        // we should not print a misleading all-zero RAM block.
        let interval_start = UNIX_EPOCH;
        let output = dump_to_string_with(id(1), render_settings(true), |tracker| {
            tracker.new_span(vec![id(1)]);
            tracker.open(
                vec![id(1)],
                closed_span(
                    "outer",
                    interval_start,
                    interval_start + Duration::from_secs(1),
                    RssSample::default(),
                    RssSample::default(),
                ),
            );
        });
        assert!(
            !output.contains("RAM:"),
            "RAM block should be absent when all samples are zero:\n{output}"
        );
    }

    #[test]
    fn span_info_for_span_samples_when_enabled() {
        // Without a real tracing Subscriber we can't call SpanInfo::for_span
        // directly, but we can exercise the gating logic the same way it does:
        // sample only when track_ram is true.
        let sampled = if true { read_rss() } else { RssSample::default() };
        let unsampled = if false { read_rss() } else { RssSample::default() };
        assert!(unsampled.is_zero());
        // On Linux the test process has live RSS; off-Linux read_rss is zero.
        #[cfg(target_os = "linux")]
        assert!(!sampled.is_zero());
        #[cfg(not(target_os = "linux"))]
        assert!(sampled.is_zero());
    }

    #[test]
    fn interest_tracker_exit_samples_when_track_ram() {
        // exit() should record a non-zero end_rss when track_ram is on, on Linux.
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
        let mut tracker = InterestTracker::new(
            id(1),
            render_settings(false),
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
        assert!(info.end_rss.is_zero());
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
