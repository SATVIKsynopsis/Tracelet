mod common;

use std::path::Path;
use std::time::Duration;

use common::{collector, repo};
use otel_test_capture::CaptureServer;
use otel_test_core::config::CollectorConfig;
use otel_test_core::runner::{self, Outcome, TestError, TestResult};
use otel_test_core::session::{self, CaptureProtocol};
use otel_test_core::spec::{self, Problem, TestCase};

async fn run_suite(spec_path: &Path) -> Vec<TestResult> {
    let c = collector().await;
    let suite = spec::load(spec_path).unwrap_or_else(|e| panic!("{e}"));
    let config = CollectorConfig::load(&suite.collector_config).unwrap();
    let capture = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &capture, CaptureProtocol::Http)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let tests: Vec<&TestCase> = suite.tests.iter().collect();
    let planned = runner::plan(&tests, &config, &prepared).unwrap_or_else(|p| panic!("{p:#?}"));
    runner::run(&planned, &prepared, c, &capture, |_| {}).await
}

async fn plan_problems(spec_path: &Path) -> Vec<Problem> {
    let c = collector().await;
    let suite = spec::load(spec_path).unwrap_or_else(|e| panic!("{e}"));
    let config = CollectorConfig::load(&suite.collector_config).unwrap();
    let capture = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &capture, CaptureProtocol::Http)
        .await
        .unwrap();
    let tests: Vec<&TestCase> = suite.tests.iter().collect();
    runner::plan(&tests, &config, &prepared).expect_err("plan should fail")
}

fn scenario(collector_yaml: &str, spec_yaml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("collector.yaml"), collector_yaml).unwrap();
    let fixtures = std::path::absolute(repo("fixtures"))
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    std::fs::write(
        dir.path().join("otel-test.yaml"),
        spec_yaml.replace("{fixtures}", &fixtures),
    )
    .unwrap();
    dir
}

fn spec_of(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("otel-test.yaml")
}

fn outcomes(results: &[TestResult]) -> Vec<(&str, &Outcome)> {
    results
        .iter()
        .map(|r| (r.name.as_str(), &r.outcome))
        .collect()
}

fn details(r: &TestResult) -> String {
    r.assertions
        .iter()
        .filter(|a| !a.passed)
        .flat_map(|a| a.details.iter().cloned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn error_message(r: &TestResult) -> &str {
    match &r.outcome {
        Outcome::Error(TestError { message, .. }) => message,
        other => panic!("{} should have errored, got {other:?}", r.name),
    }
}

const SIMPLE: &str = r#"
receivers: {otlp: {protocols: {http: {}}}}
processors:
  filter/healthz:
    error_mode: ignore
    traces: {span: ['attributes["http.route"] == "/healthz"']}
exporters: {debug: {}}
service:
  pipelines:
    traces: {receivers: [otlp], processors: [filter/healthz], exporters: [debug]}
"#;

#[tokio::test]
async fn password_redaction_example_passes() {
    let results = run_suite(&repo("examples/password-redaction/otel-test.yaml")).await;
    assert_eq!(
        outcomes(&results),
        [
            ("passwords are removed from spans", &Outcome::Passed),
            ("passwords are removed from logs", &Outcome::Passed),
            ("health checks are dropped", &Outcome::Passed),
        ],
        "{results:#?}"
    );
    assert!(results.iter().all(|r| r.duration >= Duration::from_secs(2)));
}

#[tokio::test]
async fn multi_exporter_scenario_passes() {
    let results = run_suite(&repo("tests/scenarios/multi-exporter/otel-test.yaml")).await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Passed),
        "{results:#?}"
    );
    assert_eq!(results.len(), 4);
}

#[tokio::test]
async fn broken_redaction_is_detected() {
    let collector_yaml =
        std::fs::read_to_string(repo("examples/password-redaction/collector.yaml"))
            .unwrap()
            .replace("      - key: password\n        action: delete\n", "");
    let spec_yaml = std::fs::read_to_string(repo("examples/password-redaction/otel-test.yaml"))
        .unwrap()
        .replace("fixtures/", "{fixtures}/traces/")
        .replace(
            "{fixtures}/traces/login-log.json",
            "{fixtures}/logs/login-log.json",
        );
    let dir = scenario(&collector_yaml, &spec_yaml);
    let results = run_suite(&spec_of(&dir)).await;

    assert_eq!(results[0].outcome, Outcome::Failed);
    assert_eq!(results[1].outcome, Outcome::Failed);
    assert_eq!(
        results[2].outcome,
        Outcome::Passed,
        "the health-check filter still works"
    );
    let d = details(&results[0]);
    assert!(d.contains("password = \"hunter2\""), "{d}");
    assert_eq!(
        results[0].assertions.iter().filter(|a| !a.passed).count(),
        1
    );
}

#[tokio::test]
async fn positive_tests_finish_early_and_missing_telemetry_waits_the_full_timeout() {
    let dir = scenario(
        SIMPLE,
        r#"
collector: collector.yaml
tests:
  - name: reaches debug quickly
    input: "{fixtures}/traces/app-span.json"
    timeout: 20s
    expect:
      - exporter: debug
  - name: filtered span never arrives
    input: "{fixtures}/traces/healthz-span.json"
    timeout: 1500ms
    expect:
      - exporter: debug
"#,
    );
    let results = run_suite(&spec_of(&dir)).await;

    assert_eq!(results[0].outcome, Outcome::Passed);
    assert!(
        results[0].duration < Duration::from_secs(5),
        "took {:?}",
        results[0].duration
    );

    assert_eq!(results[1].outcome, Outcome::Failed);
    assert!(
        results[1].duration >= Duration::from_millis(1500),
        "took {:?}",
        results[1].duration
    );
    let d = details(&results[1]);
    assert!(
        d.contains("Expected: at least 1 span\nActual:   0 spans\nExporter: debug"),
        "{d}"
    );
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "Windows has no graceful stop signal: the Collector is killed and buffered telemetry is lost"
)]
async fn buffered_telemetry_is_flushed_into_its_own_test() {
    let dir = scenario(
        r#"
receivers: {otlp: {protocols: {http: {}}}}
processors:
  filter/healthz:
    error_mode: ignore
    traces: {span: ['attributes["http.route"] == "/healthz"']}
  batch: {timeout: 30s, send_batch_size: 10000}
exporters: {debug: {}}
service:
  pipelines:
    traces: {receivers: [otlp], processors: [filter/healthz, batch], exporters: [debug]}
"#,
        r#"
collector: collector.yaml
defaults: {timeout: 500ms}
tests:
  - name: batched span is still counted
    input: "{fixtures}/traces/app-span.json"
    expect:
      - {exporter: debug, count: 1}
  - name: next test is not contaminated
    input: "{fixtures}/traces/healthz-span.json"
    expect:
      - dropped: true
"#,
    );
    let results = run_suite(&spec_of(&dir)).await;
    assert_eq!(
        outcomes(&results),
        [
            ("batched span is still counted", &Outcome::Passed),
            ("next test is not contaminated", &Outcome::Passed),
        ],
        "{results:#?}"
    );
    assert!(
        results[0]
            .notes
            .iter()
            .any(|n| n.contains("1 span arrived during Collector shutdown")),
        "{:?}",
        results[0].notes
    );
    assert!(results[1].warnings.is_empty(), "{:?}", results[1].warnings);
}

#[tokio::test]
async fn injection_failure_is_an_error_not_a_drop() {
    let dir = scenario(
        SIMPLE,
        r#"
collector: collector.yaml
tests:
  - name: would look dropped
    input: "{fixtures}/traces/app-span.json"
    expect:
      - dropped: true
"#,
    );
    let c = collector().await;
    let suite = spec::load(&spec_of(&dir)).unwrap();
    let config = CollectorConfig::load(&suite.collector_config).unwrap();
    let target = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &target, CaptureProtocol::Http)
        .await
        .unwrap();
    let tests: Vec<&TestCase> = suite.tests.iter().collect();
    let planned = runner::plan(&tests, &config, &prepared).unwrap();
    target.shutdown().await;
    let observer = CaptureServer::start().await.unwrap();

    let results = runner::run(&planned, &prepared, c, &observer, |_| {}).await;
    let message = error_message(&results[0]);
    assert!(message.contains("the Collector rejected"), "{message}");
    assert!(message.contains("HTTP 5"), "{message}");
    assert!(
        results[0].assertions.is_empty(),
        "no assertions may be evaluated"
    );
}

#[tokio::test]
async fn startup_failure_stops_the_suite() {
    let dir = scenario(
        SIMPLE,
        r#"
collector: collector.yaml
tests:
  - name: first
    input: "{fixtures}/traces/app-span.json"
    expect: [{exporter: debug}]
  - name: second
    input: "{fixtures}/traces/app-span.json"
    expect: [{exporter: debug}]
"#,
    );
    let c = collector().await;
    let suite = spec::load(&spec_of(&dir)).unwrap();
    let config = CollectorConfig::load(&suite.collector_config).unwrap();
    let capture = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &capture, CaptureProtocol::Http)
        .await
        .unwrap();
    let tests: Vec<&TestCase> = suite.tests.iter().collect();
    let planned = runner::plan(&tests, &config, &prepared).unwrap();
    let _blocker = std::net::TcpListener::bind(("127.0.0.1", planned[0].injection_port)).unwrap();

    let results = runner::run(&planned, &prepared, c, &capture, |_| {}).await;
    let Outcome::Error(first) = &results[0].outcome else {
        panic!("{results:#?}")
    };
    assert!(first.startup);
    assert!(
        first.message.contains("exited during startup"),
        "{}",
        first.message
    );
    assert!(
        results[0]
            .collector_logs
            .iter()
            .any(|l| l.contains("address already in use"))
    );
    let Outcome::Error(second) = &results[1].outcome else {
        panic!("{results:#?}")
    };
    assert!(
        second.startup && second.message.starts_with("not run"),
        "{second:?}"
    );
}

#[tokio::test]
async fn plan_reports_spec_and_config_mismatches() {
    let dir = scenario(
        &std::fs::read_to_string(repo("tests/scenarios/multi-exporter/collector.yaml")).unwrap(),
        r#"
collector: collector.yaml
tests:
  - name: ambiguous logs receiver
    input: "{fixtures}/logs/login-log.json"
    expect: [{exporter: debug}]
  - name: unknown exporter
    input: "{fixtures}/traces/app-span.json"
    expect: [{exporter: kafka, count: 1}, {attribute: x, present: true, exporter: datadog}]
  - name: connector is not an exporter
    input: "{fixtures}/traces/app-span.json"
    expect: [{exporter: routing/errors}]
  - name: wrong signal
    input: "{fixtures}/traces/app-span.json"
    signal: metrics
    expect: [{dropped: true}]
  - name: receiver without that signal
    input: "{fixtures}/traces/app-span.json"
    receiver: filelog/app
    expect: [{dropped: true}]
"#,
    );
    let problems = plan_problems(&spec_of(&dir)).await;
    let got: Vec<String> = problems
        .iter()
        .map(|p| format!("{}: {}", p.location, p.message))
        .collect();
    assert_eq!(got.len(), 5, "{got:#?}");
    assert!(got[0].starts_with("test \"ambiguous logs receiver\".receiver: several receivers feed logs pipelines (otlp, filelog/app)"), "{}", got[0]);
    assert!(
        got[1].starts_with(
            "test \"unknown exporter\".expect[0]: exporter \"kafka\" is not used by any pipeline"
        ),
        "{}",
        got[1]
    );
    assert!(
        got[2].starts_with(
            "test \"connector is not an exporter\".expect[0]: \"routing/errors\" is a connector"
        ),
        "{}",
        got[2]
    );
    assert!(
        got[3].contains("declares `signal: metrics` but the fixture contains traces"),
        "{}",
        got[3]
    );
    assert!(
        got[4].contains("receiver \"filelog/app\" does not feed any traces pipeline"),
        "{}",
        got[4]
    );
}

#[tokio::test]
async fn metrics_end_to_end() {
    let dir = scenario(
        r#"
receivers: {otlp: {protocols: {http: {}}}}
processors:
  # Drop the data point for failed requests.
  filter/errors:
    error_mode: ignore
    metrics:
      datapoint: ['attributes["http.response.status_code"] == 500']
exporters: {debug: {}}
service:
  pipelines:
    metrics: {receivers: [otlp], processors: [filter/errors], exporters: [debug]}
"#,
        r#"
collector: collector.yaml
defaults: {timeout: 1s}
tests:
  - name: error data point is filtered
    input: "{fixtures}/metrics/http-requests.json"
    expect:
      - {exporter: debug, count: 1}
      - {attribute: http.response.status_code, equals: 200}
      - {attribute: service.name, level: resource, equals: checkout}
  - name: unfiltered count would be wrong
    input: "{fixtures}/metrics/http-requests.json"
    expect:
      - {exporter: debug, count: 2}
"#,
    );
    let results = run_suite(&spec_of(&dir)).await;
    assert_eq!(results[0].outcome, Outcome::Passed, "{results:#?}");
    assert_eq!(results[1].outcome, Outcome::Failed);
    let d = details(&results[1]);
    assert!(
        d.contains("Expected: 2 data points\nActual:   1 data point\nExporter: debug"),
        "{d}"
    );
}
