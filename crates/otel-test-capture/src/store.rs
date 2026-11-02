use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl Signal {
    pub fn path_segment(self) -> &'static str {
        match self {
            Signal::Traces => "traces",
            Signal::Metrics => "metrics",
            Signal::Logs => "logs",
        }
    }

    pub(crate) fn from_path_segment(s: &str) -> Option<Self> {
        [Signal::Traces, Signal::Metrics, Signal::Logs]
            .into_iter()
            .find(|sig| sig.path_segment() == s)
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.path_segment())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Grpc,
    HttpProtobuf,
    HttpJson,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Traces(ExportTraceServiceRequest),
    Metrics(ExportMetricsServiceRequest),
    Logs(ExportLogsServiceRequest),
}

impl Payload {
    pub fn signal(&self) -> Signal {
        match self {
            Payload::Traces(_) => Signal::Traces,
            Payload::Metrics(_) => Signal::Metrics,
            Payload::Logs(_) => Signal::Logs,
        }
    }

    pub fn item_count(&self) -> usize {
        match self {
            Payload::Traces(r) => r
                .resource_spans
                .iter()
                .flat_map(|rs| &rs.scope_spans)
                .map(|ss| ss.spans.len())
                .sum(),
            Payload::Logs(r) => r
                .resource_logs
                .iter()
                .flat_map(|rl| &rl.scope_logs)
                .map(|sl| sl.log_records.len())
                .sum(),
            Payload::Metrics(r) => r
                .resource_metrics
                .iter()
                .flat_map(|rm| &rm.scope_metrics)
                .flat_map(|sm| &sm.metrics)
                .map(|m| match &m.data {
                    Some(Data::Gauge(g)) => g.data_points.len(),
                    Some(Data::Sum(s)) => s.data_points.len(),
                    Some(Data::Histogram(h)) => h.data_points.len(),
                    Some(Data::ExponentialHistogram(h)) => h.data_points.len(),
                    Some(Data::Summary(s)) => s.data_points.len(),
                    None => 0,
                })
                .sum(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Captured {
    pub exporter: String,
    pub protocol: Protocol,
    pub received_at: Instant,
    pub payload: Payload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    #[error("capture window {open:?} is still open; close it before opening {requested:?}")]
    AlreadyOpen { open: String, requested: String },
}

#[derive(Debug, Default)]
struct State {
    bindings: HashMap<String, String>,
    window: Option<OpenWindow>,
    next_window: u64,
    unattributed: Vec<Captured>,
    rejected: Vec<Rejected>,
}

#[derive(Debug)]
struct OpenWindow {
    id: u64,
    label: String,
    items: Vec<Captured>,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    changes: watch::Sender<u64>,
}

#[derive(Debug, Clone)]
pub struct Store(Arc<Inner>);

impl Default for Store {
    fn default() -> Self {
        Store(Arc::new(Inner {
            state: Mutex::default(),
            changes: watch::Sender::new(0),
        }))
    }
}

impl Store {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn register(&self, slug: impl Into<String>, exporter: impl Into<String>) {
        self.state().bindings.insert(slug.into(), exporter.into());
    }

    pub(crate) fn exporter_for(&self, slug: &str) -> Option<String> {
        self.state().bindings.get(slug).cloned()
    }

    pub(crate) fn record(&self, exporter: String, protocol: Protocol, payload: Payload) {
        let item = Captured {
            exporter,
            protocol,
            received_at: Instant::now(),
            payload,
        };
        {
            let mut state = self.state();
            match &mut state.window {
                Some(w) => w.items.push(item),
                None => {
                    tracing::debug!(exporter = %item.exporter, "telemetry received outside any capture window");
                    state.unattributed.push(item);
                }
            }
        }
        self.0.changes.send_modify(|n| *n += 1);
    }

    pub(crate) fn reject(&self, reason: String) {
        tracing::warn!(%reason, "rejected capture request");
        self.state().rejected.push(Rejected { reason });
        self.0.changes.send_modify(|n| *n += 1);
    }

    pub fn open_window(&self, label: impl Into<String>) -> Result<Window, WindowError> {
        let label = label.into();
        let mut state = self.state();
        if let Some(open) = &state.window {
            return Err(WindowError::AlreadyOpen {
                open: open.label.clone(),
                requested: label,
            });
        }
        state.next_window += 1;
        let id = state.next_window;
        state.window = Some(OpenWindow {
            id,
            label,
            items: Vec::new(),
        });
        Ok(Window {
            store: self.clone(),
            id,
            changes: self.0.changes.subscribe(),
            closed: false,
        })
    }

    pub fn take_unattributed(&self) -> Vec<Captured> {
        std::mem::take(&mut self.state().unattributed)
    }

    pub fn take_rejected(&self) -> Vec<Rejected> {
        std::mem::take(&mut self.state().rejected)
    }
}

#[derive(Debug)]
pub struct Window {
    store: Store,
    id: u64,
    changes: watch::Receiver<u64>,
    closed: bool,
}

impl Window {
    pub fn snapshot(&self) -> Vec<Captured> {
        match &self.store.state().window {
            Some(w) if w.id == self.id => w.items.clone(),
            _ => Vec::new(),
        }
    }

    pub async fn changed(&mut self) {
        let _ = self.changes.changed().await;
    }

    pub fn close(mut self) -> Vec<Captured> {
        self.take()
    }

    fn take(&mut self) -> Vec<Captured> {
        self.closed = true;
        let mut state = self.store.state();
        match state.window.take() {
            Some(w) if w.id == self.id => w.items,
            other => {
                state.window = other;
                Vec::new()
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if !self.closed {
            self.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::metrics::v1::{
        Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
        ScopeMetrics, Sum,
    };
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
    use std::time::Duration;

    pub(crate) fn spans(n: usize) -> Payload {
        Payload::Traces(ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span::default(); n],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        })
    }

    #[test]
    fn item_counts() {
        assert_eq!(spans(3).item_count(), 3);
        let two_resources = Payload::Traces(ExportTraceServiceRequest {
            resource_spans: vec![
                ResourceSpans {
                    scope_spans: vec![
                        ScopeSpans {
                            spans: vec![Span::default(); 2],
                            ..Default::default()
                        },
                        ScopeSpans::default(),
                    ],
                    ..Default::default()
                },
                ResourceSpans {
                    scope_spans: vec![ScopeSpans {
                        spans: vec![Span::default()],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
        });
        assert_eq!(two_resources.item_count(), 3);

        let logs = Payload::Logs(ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord::default(); 4],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        });
        assert_eq!((logs.signal(), logs.item_count()), (Signal::Logs, 4));

        let metric = |data| Metric {
            data: Some(data),
            ..Default::default()
        };
        let metrics = Payload::Metrics(ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![
                        metric(Data::Gauge(Gauge {
                            data_points: vec![NumberDataPoint::default(); 2],
                        })),
                        metric(Data::Sum(Sum {
                            data_points: vec![NumberDataPoint::default()],
                            ..Default::default()
                        })),
                        metric(Data::Histogram(Histogram {
                            data_points: vec![HistogramDataPoint::default(); 3],
                            ..Default::default()
                        })),
                        Metric::default(),
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        });
        assert_eq!(
            (metrics.signal(), metrics.item_count()),
            (Signal::Metrics, 6)
        );
    }

    #[test]
    fn windows_isolate_telemetry() {
        let store = Store::default();
        store.record("before".into(), Protocol::Grpc, spans(1));

        let w = store.open_window("test 1").unwrap();
        store.record("datadog".into(), Protocol::HttpProtobuf, spans(2));
        assert_eq!(w.snapshot().len(), 1);
        let items = w.close();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].exporter, "datadog");
        assert_eq!(items[0].payload.item_count(), 2);

        store.record("late".into(), Protocol::Grpc, spans(1));
        let w2 = store.open_window("test 2").unwrap();
        assert!(w2.snapshot().is_empty(), "a new window starts empty");
        drop(w2);

        let stray: Vec<_> = store
            .take_unattributed()
            .into_iter()
            .map(|c| c.exporter)
            .collect();
        assert_eq!(stray, ["before", "late"]);
        assert!(store.take_unattributed().is_empty());
    }

    #[test]
    fn only_one_window_at_a_time() {
        let store = Store::default();
        let w = store.open_window("a").unwrap();
        let err = store.open_window("b").unwrap_err();
        assert_eq!(
            err.to_string(),
            "capture window \"a\" is still open; close it before opening \"b\""
        );
        drop(w);
        let w = store.open_window("b").unwrap();
        store.record("x".into(), Protocol::Grpc, spans(1));
        drop(w);
        assert!(
            store.take_unattributed().is_empty(),
            "dropped window's data is discarded, not leaked"
        );
    }

    #[test]
    fn registration_and_rejections() {
        let store = Store::default();
        store.register("otlp_http_backend", "otlp_http/backend");
        assert_eq!(
            store.exporter_for("otlp_http_backend").as_deref(),
            Some("otlp_http/backend")
        );
        assert_eq!(store.exporter_for("nope"), None);
        store.reject("unknown exporter".into());
        assert_eq!(
            store.take_rejected(),
            [Rejected {
                reason: "unknown exporter".into()
            }]
        );
    }

    #[tokio::test]
    async fn changed_wakes_on_new_data() {
        let store = Store::default();
        let mut w = store.open_window("t").unwrap();
        let writer = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            writer.record("debug".into(), Protocol::Grpc, spans(1));
        });
        tokio::time::timeout(Duration::from_secs(5), w.changed())
            .await
            .expect("woken by record");
        assert_eq!(w.snapshot().len(), 1);
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        let store = Store::default();
        let w = store.open_window("t").unwrap();
        std::thread::scope(|s| {
            for t in 0..8 {
                let store = store.clone();
                s.spawn(move || {
                    for _ in 0..250 {
                        store.record(format!("e{t}"), Protocol::Grpc, spans(1));
                    }
                });
            }
        });
        assert_eq!(w.close().len(), 2000);
    }
}
