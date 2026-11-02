use std::time::{Duration, Instant};

use otel_test_capture::{CaptureServer, Captured};

use crate::assert::{self, AssertionResult, ExporterSignals};
use crate::collector::{Collector, CollectorError};
use crate::config::{self, CollectorConfig};
use crate::duration;
use crate::fixture::{Fixture, signal_items};
use crate::session::{DEFAULT_STARTUP_TIMEOUT, Prepared, SHUTDOWN_GRACE};
use crate::spec::{Assertion, AttributeCheck, Problem, Signal, TestCase};

pub const SETTLE: Duration = Duration::from_millis(500);
const INJECT_TIMEOUT: Duration = Duration::from_secs(30);
const GROUPBYTRACE_DEFAULT_WAIT: Duration = Duration::from_secs(1);
const PERIODIC_CONNECTORS: &[(&str, Option<Duration>)] = &[
    ("span_metrics", Some(Duration::from_secs(60))),
    ("spanmetrics", Some(Duration::from_secs(60))),
    ("service_graph", None),
    ("servicegraph", None),
];

#[derive(Debug, Clone)]
pub struct PlannedTest {
    pub test: TestCase,
    pub fixture: Fixture,
    pub receiver: String,
    pub injection_port: u16,
    pub exporter_signals: ExporterSignals,
    pub warnings: Vec<String>,
}

pub fn plan(
    tests: &[&TestCase],
    config: &CollectorConfig,
    prepared: &Prepared,
) -> Result<Vec<PlannedTest>, Vec<Problem>> {
    let rewritten = &prepared.rewritten;
    let mut problems = Vec::new();
    let mut planned = Vec::new();
    for test in tests {
        let loc = format!("test {:?}", test.name);
        let fixture = match Fixture::load(&test.input, test.signal) {
            Ok(f) => f,
            Err(e) => {
                problems.push(Problem {
                    location: format!("{loc}.input"),
                    message: e.to_string(),
                });
                continue;
            }
        };
        let injection = match rewritten.injection_for(fixture.signal, test.receiver.as_deref()) {
            Ok(i) => i,
            Err(message) => {
                problems.push(Problem {
                    location: format!("{loc}.receiver"),
                    message,
                });
                continue;
            }
        };
        for (j, a) in test.expect.iter().enumerate() {
            let exporter = match a {
                Assertion::ExporterReached { exporter }
                | Assertion::ExporterCount { exporter, .. } => Some(exporter),
                Assertion::Attribute(a) => a.exporter.as_ref(),
                Assertion::Dropped => None,
            };
            if let Some(e) = exporter
                && rewritten.exporter(e).is_none()
            {
                let known: Vec<&str> = rewritten
                    .exporters
                    .iter()
                    .map(|x| x.original.as_str())
                    .collect();
                let message = if config.is_connector(e) {
                    format!(
                        "{e:?} is a connector, not an exporter; assert on the exporters of the pipelines it feeds"
                    )
                } else {
                    format!(
                        "exporter {e:?} is not used by any pipeline (exporters: {})",
                        known.join(", ")
                    )
                };
                problems.push(Problem {
                    location: format!("{loc}.expect[{j}]"),
                    message,
                });
            }
        }
        let warnings = timing_warnings(config, &injection.original, fixture.signal, test.timeout);
        planned.push(PlannedTest {
            test: (*test).clone(),
            receiver: injection.original.clone(),
            injection_port: injection.port,
            exporter_signals: rewritten
                .exporters
                .iter()
                .map(|e| (e.original.clone(), e.signals.clone()))
                .collect(),
            fixture,
            warnings,
        });
    }
    if problems.is_empty() {
        Ok(planned)
    } else {
        Err(problems)
    }
}

fn timing_warnings(
    config: &CollectorConfig,
    receiver: &str,
    signal: Signal,
    timeout: Duration,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut seen = Vec::new();
    for pipeline in config.reachable_pipelines(receiver, signal) {
        for p in &pipeline.processors {
            if seen.contains(p) || config::split_id(p).0 != "groupbytrace" {
                continue;
            }
            seen.push(p.clone());
            let raw = config
                .component("processors", p)
                .and_then(|c| c.get("wait_duration"))
                .and_then(|v| v.as_str());
            let wait = match raw {
                None => GROUPBYTRACE_DEFAULT_WAIT,
                Some(s) => match duration::parse_go(s) {
                    Some(d) => d,
                    None => {
                        warnings.push(format!(
                            "processor {p}: could not interpret wait_duration {s:?}; if it is not shorter than the {}ms timeout, its output will be lost at shutdown",
                            timeout.as_millis()
                        ));
                        continue;
                    }
                },
            };
            if wait >= timeout {
                warnings.push(format!(
                    "processor {p} holds traces for {}ms, which is not shorter than this test's {}ms timeout, and discards them at shutdown; raise `timeout` above {}ms or results are meaningless",
                    wait.as_millis(),
                    timeout.as_millis(),
                    wait.as_millis()
                ));
            }
        }
        for c in &pipeline.exporters {
            let Some((_, default)) = PERIODIC_CONNECTORS
                .iter()
                .find(|(t, _)| config.is_connector(c) && config::split_id(c).0 == *t)
            else {
                continue;
            };
            if seen.contains(c) {
                continue;
            }
            seen.push(c.clone());
            let raw = config
                .component("connectors", c)
                .and_then(|v| v.get("metrics_flush_interval"))
                .and_then(|v| v.as_str());
            let interval = match (raw, default) {
                (Some(s), _) => match duration::parse_go(s) {
                    Some(d) => d,
                    None => {
                        warnings.push(format!(
                            "connector {c}: could not interpret metrics_flush_interval {s:?}; it emits metrics only on that timer and not at shutdown, so it must be shorter than the {}ms timeout",
                            timeout.as_millis()
                        ));
                        continue;
                    }
                },
                (None, Some(d)) => *d,
                (None, None) => {
                    warnings.push(format!(
                        "connector {c} has no metrics_flush_interval and does not emit at shutdown, so its metrics will not be captured; set metrics_flush_interval below this test's {}ms timeout",
                        timeout.as_millis()
                    ));
                    continue;
                }
            };
            if interval >= timeout {
                warnings.push(format!(
                    "connector {c} emits metrics every {}ms, which is not shorter than this test's {}ms timeout, and does not emit at shutdown; its metrics will not be captured. Set metrics_flush_interval below the timeout",
                    interval.as_millis(),
                    timeout.as_millis()
                ));
            }
        }
    }
    warnings
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Error(TestError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestError {
    pub startup: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub signal: Signal,
    pub outcome: Outcome,
    pub assertions: Vec<AssertionResult>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
    pub duration: Duration,
    pub collector_logs: Vec<String>,
}

pub async fn run(
    planned: &[PlannedTest],
    prepared: &Prepared,
    collector: &Collector,
    capture: &CaptureServer,
    mut on_result: impl FnMut(&TestResult),
) -> Vec<TestResult> {
    let mut results = Vec::with_capacity(planned.len());
    let mut startup_failed = false;
    for p in planned {
        let result = if startup_failed {
            TestResult {
                name: p.test.name.clone(),
                signal: p.fixture.signal,
                outcome: Outcome::Error(TestError {
                    startup: true,
                    message: "not run: the Collector failed to start for a previous test".into(),
                }),
                assertions: vec![],
                warnings: p.warnings.clone(),
                notes: vec![],
                duration: Duration::ZERO,
                collector_logs: vec![],
            }
        } else {
            run_one(p, prepared, collector, capture).await
        };
        if let Outcome::Error(TestError { startup: true, .. }) = result.outcome {
            startup_failed = true;
        }
        on_result(&result);
        results.push(result);
    }
    results
}

async fn run_one(
    p: &PlannedTest,
    prepared: &Prepared,
    collector: &Collector,
    capture: &CaptureServer,
) -> TestResult {
    let started = Instant::now();
    let mut result = TestResult {
        name: p.test.name.clone(),
        signal: p.fixture.signal,
        outcome: Outcome::Passed,
        assertions: vec![],
        warnings: p.warnings.clone(),
        notes: vec![],
        duration: Duration::ZERO,
        collector_logs: vec![],
    };
    let error = |startup: bool, message: String| Outcome::Error(TestError { startup, message });

    report_unattributed(capture, &mut result.warnings, "before this test started");
    let _ = capture.store().take_rejected();

    let mut running = match prepared.start(collector, DEFAULT_STARTUP_TIMEOUT).await {
        Ok(r) => r,
        Err(e) => {
            let logs = match &e {
                CollectorError::Exited { logs, .. } | CollectorError::NotReady { logs, .. } => {
                    logs.clone()
                }
                _ => vec![],
            };
            result.collector_logs = logs;
            let full = e.to_string();
            let (summary, rest) = full.split_once('\n').unwrap_or((&full, ""));
            let mut message = summary.to_owned();
            for hint in &prepared.rewritten.startup_hints {
                message.push_str("\nhint: ");
                message.push_str(hint);
            }
            if !rest.is_empty() {
                message.push('\n');
                message.push_str(rest);
            }
            result.outcome = error(true, message);
            result.duration = started.elapsed();
            return result;
        }
    };

    let mut window = match capture.store().open_window(p.test.name.clone()) {
        Ok(w) => w,
        Err(e) => {
            let _ = running.stop(SHUTDOWN_GRACE).await;
            result.outcome = error(false, e.to_string());
            return result;
        }
    };

    let mut failure: Option<String> = None;
    if let Err(e) = inject(p).await {
        failure = Some(e);
    }

    let window_start = Instant::now();
    if failure.is_none() {
        let deadline = window_start + p.test.timeout;
        let full_window = p.test.expect.iter().any(needs_full_window);
        let settle = SETTLE.min(p.test.timeout);
        let mut seen = 0;
        let mut last_arrival = Instant::now();
        loop {
            match running.exit_status() {
                Ok(Some(status)) => {
                    failure = Some(format!(
                        "the Collector exited unexpectedly during the test ({status})"
                    ));
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    failure = Some(format!("failed to check the Collector process: {e}"));
                    break;
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let snapshot = window.snapshot();
            if snapshot.len() != seen {
                seen = snapshot.len();
                last_arrival = now;
            }
            if !full_window
                && !snapshot.is_empty()
                && now.duration_since(last_arrival) >= settle
                && all_pass(p, &snapshot)
            {
                break;
            }
            let wake = (deadline - now).min(Duration::from_millis(100));
            let _ = tokio::time::timeout(wake, window.changed()).await;
        }
    }

    let before_shutdown = window.snapshot().len();
    match running.shutdown(SHUTDOWN_GRACE).await {
        Ok((stopped, logs)) => {
            result.collector_logs = logs;
            if let Err(e) = stopped.check()
                && failure.is_none()
            {
                failure = Some(format!("{e} during shutdown"));
            }
        }
        Err(e) => failure = failure.or(Some(e.to_string())),
    }
    let captured = window.close();
    result.duration = started.elapsed();

    let flushed = captured.len().saturating_sub(before_shutdown);
    if flushed > 0 {
        let items: usize = captured[before_shutdown..]
            .iter()
            .map(|c| c.payload.item_count())
            .sum();
        result.notes.push(format!(
            "{items} {} arrived during Collector shutdown (flushed by a buffering processor)",
            signal_items(p.fixture.signal, items)
        ));
    }
    let rejected = capture.store().take_rejected();
    if !rejected.is_empty() && failure.is_none() {
        let reasons: Vec<String> = rejected.into_iter().map(|r| r.reason).collect();
        failure = Some(format!(
            "the capture server rejected {} request(s) from the Collector: {}",
            reasons.len(),
            reasons.join("; ")
        ));
    }

    if let Some(message) = failure {
        result.outcome = error(false, message);
        return result;
    }
    result.assertions = p
        .test
        .expect
        .iter()
        .map(|a| assert::evaluate(a, p.fixture.signal, &p.exporter_signals, &captured))
        .collect();
    result.outcome = if result.assertions.iter().all(|a| a.passed) {
        Outcome::Passed
    } else {
        Outcome::Failed
    };
    result
}

fn report_unattributed(capture: &CaptureServer, warnings: &mut Vec<String>, when: &str) {
    let stray = capture.store().take_unattributed();
    if !stray.is_empty() {
        let items: usize = stray.iter().map(|c| c.payload.item_count()).sum();
        warnings.push(format!("{items} item(s) arrived {when} and were ignored"));
    }
}

fn needs_full_window(a: &Assertion) -> bool {
    match a {
        Assertion::Dropped | Assertion::ExporterCount { .. } => true,
        Assertion::Attribute(a) => matches!(a.check, AttributeCheck::Absent),
        Assertion::ExporterReached { .. } => false,
    }
}

fn all_pass(p: &PlannedTest, captured: &[Captured]) -> bool {
    p.test
        .expect
        .iter()
        .all(|a| assert::evaluate(a, p.fixture.signal, &p.exporter_signals, captured).passed)
}

async fn inject(p: &PlannedTest) -> Result<(), String> {
    let url = format!(
        "http://127.0.0.1:{}/v1/{}",
        p.injection_port, p.fixture.signal
    );
    let response = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .timeout(INJECT_TIMEOUT)
        .body(p.fixture.body.clone())
        .send()
        .await
        .map_err(|e| {
            format!(
                "failed to inject {} into receiver {}: {e}",
                p.fixture.path.display(),
                p.receiver
            )
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "the Collector rejected {} via receiver {} (HTTP {status}): {}",
            p.fixture.path.display(),
            p.receiver,
            body.trim()
        ));
    }
    if let Some(rejection) = partial_success_rejection(&body) {
        return Err(format!(
            "the Collector partially rejected {} via receiver {}: {rejection}",
            p.fixture.path.display(),
            p.receiver
        ));
    }
    Ok(())
}

fn partial_success_rejection(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let ps = v.get("partialSuccess")?;
    let rejected = ["rejectedSpans", "rejectedLogRecords", "rejectedDataPoints"]
        .iter()
        .find_map(|k| ps.get(*k))
        .and_then(|n| n.as_i64().or_else(|| n.as_str()?.parse().ok()))
        .unwrap_or(0);
    let message = ps
        .get("errorMessage")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    (rejected > 0).then(|| format!("{rejected} item(s) rejected: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn partial_success_detection() {
        assert_eq!(partial_success_rejection(r#"{"partialSuccess":{}}"#), None);
        assert_eq!(partial_success_rejection("{}"), None);
        assert_eq!(partial_success_rejection("not json"), None);
        assert_eq!(
            partial_success_rejection(
                r#"{"partialSuccess":{"rejectedSpans":"2","errorMessage":"bad"}}"#
            )
            .as_deref(),
            Some("2 item(s) rejected: bad")
        );
        assert_eq!(
            partial_success_rejection(r#"{"partialSuccess":{"rejectedLogRecords":1}}"#).as_deref(),
            Some("1 item(s) rejected: ")
        );
    }

    #[test]
    fn full_window_rules() {
        let spec = "collector: c\ntests:\n  - name: t\n    input: f\n    expect:\n      - {dropped: true}\n      - {exporter: e, count: 0}\n      - {attribute: a, absent: true}\n      - {exporter: e}\n      - {attribute: a, present: true}\n      - {attribute: a, equals: 1}\n";
        let suite = crate::spec::parse(spec, Path::new("s.yaml")).unwrap();
        let rules: Vec<bool> = suite.tests[0]
            .expect
            .iter()
            .map(needs_full_window)
            .collect();
        assert_eq!(rules, [true, true, true, false, false, false]);
    }

    #[test]
    fn groupbytrace_warnings() {
        let config = CollectorConfig::parse(
            r#"
receivers: {otlp: }
processors:
  groupbytrace/slow: {wait_duration: 5s}
  groupbytrace: {}
  groupbytrace/weird: {wait_duration: "${env:WAIT}"}
  batch: {timeout: 30s}
connectors: {forward: }
exporters: {debug: }
service:
  pipelines:
    traces: {receivers: [otlp], processors: [batch, groupbytrace], exporters: [forward]}
    traces/2: {receivers: [forward], processors: [groupbytrace/slow, groupbytrace/weird], exporters: [debug]}
    logs: {receivers: [otlp], exporters: [debug]}
"#,
            Path::new("c.yaml"),
        )
        .unwrap();
        let w = timing_warnings(&config, "otlp", Signal::Traces, Duration::from_secs(3));
        assert_eq!(w.len(), 2, "{w:#?}");
        assert!(
            w[0].starts_with("processor groupbytrace/slow holds traces for 5000ms"),
            "{}",
            w[0]
        );
        assert!(
            w[1].contains("could not interpret wait_duration \"${env:WAIT}\""),
            "{}",
            w[1]
        );
        let w = timing_warnings(&config, "otlp", Signal::Traces, Duration::from_secs(1));
        assert!(
            w.iter()
                .any(|w| w.starts_with("processor groupbytrace holds traces for 1000ms")),
            "{w:#?}"
        );
        assert!(
            timing_warnings(&config, "otlp", Signal::Logs, Duration::from_millis(100)).is_empty()
        );
    }

    #[test]
    fn periodic_connector_warnings() {
        let config = CollectorConfig::parse(
            r#"
receivers: {otlp: }
connectors:
  span_metrics: {}
  spanmetrics/fast: {metrics_flush_interval: 200ms}
  service_graph: {}
  service_graph/tuned: {metrics_flush_interval: 1s}
exporters: {debug: }
service:
  pipelines:
    traces: {receivers: [otlp], exporters: [span_metrics, spanmetrics/fast, service_graph, service_graph/tuned]}
    metrics: {receivers: [span_metrics, spanmetrics/fast, service_graph, service_graph/tuned], exporters: [debug]}
"#,
            Path::new("c.yaml"),
        )
        .unwrap();
        let w = timing_warnings(&config, "otlp", Signal::Traces, Duration::from_secs(2));
        assert_eq!(w.len(), 2, "{w:#?}");
        assert!(
            w[0].starts_with("connector span_metrics emits metrics every 60000ms"),
            "{}",
            w[0]
        );
        assert!(
            w[1].starts_with("connector service_graph has no metrics_flush_interval"),
            "{}",
            w[1]
        );
        let w = timing_warnings(&config, "otlp", Signal::Traces, Duration::from_secs(1));
        assert!(
            w.iter()
                .any(|w| w.starts_with("connector service_graph/tuned emits metrics every 1000ms")),
            "{w:#?}"
        );
    }
}
