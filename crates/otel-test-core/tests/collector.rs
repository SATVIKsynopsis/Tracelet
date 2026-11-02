mod common;

use std::path::Path;
use std::time::Duration;

use opentelemetry_proto::tonic::common::v1::KeyValue;
use otel_test_capture::{CaptureServer, Captured, Payload, Window};
use otel_test_core::collector::{Collector, CollectorError, ReadinessProbe};
use otel_test_core::config::CollectorConfig;
use otel_test_core::rewrite::{self, CaptureTarget, RewriteOptions};
use otel_test_core::session::{
    self, CaptureProtocol, DEFAULT_STARTUP_TIMEOUT, PrepareError, Prepared, SHUTDOWN_GRACE,
};
use otel_test_core::spec::Signal;
use serde_yaml::Value;

use common::{collector, repo};

fn write_config(dir: &Path, yaml: &str) -> CollectorConfig {
    let path = dir.join("collector.yaml");
    std::fs::write(&path, yaml).unwrap();
    CollectorConfig::load(&path).unwrap()
}

const SYNC_CONFIG: &str = r#"
receivers:
  otlp: {protocols: {grpc: {}}}
processors:
  attributes/redact:
    actions: [{key: password, action: delete}]
exporters:
  otlp_http/backend: {endpoint: https://otlp.example.com}
service:
  pipelines:
    traces:
      receivers: [otlp]
      processors: [attributes/redact]
      exporters: [otlp_http/backend]
"#;

async fn print_config(collector: &Collector, config: &Path) -> Value {
    let out = tokio::process::Command::new(collector.path())
        .arg("print-config")
        .arg(format!("--config=file:{}", config.display()))
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_yaml::from_slice(&out.stdout).unwrap()
}

#[tokio::test]
async fn probes_pinned_collector() {
    let c = collector().await;
    assert!(!c.version().is_empty());
    assert_eq!(c.otlp_http_exporter().unwrap(), "otlp_http");
    assert!(c.has_health_check());
}

#[tokio::test]
async fn rewritten_multi_exporter_config_validates_and_preserves_pipelines() {
    let c = collector().await;
    let config =
        CollectorConfig::load(&repo("tests/scenarios/multi-exporter/collector.yaml")).unwrap();
    let cap = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &cap, CaptureProtocol::Http)
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let resolved = print_config(c, &prepared.config_path).await;
    let pipelines = &resolved["service"]["pipelines"];
    let list = |pl: &str, key: &str| -> Vec<String> {
        pipelines[pl][key]
            .as_sequence()
            .map(|s| s.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default()
    };
    for pl in &config.pipelines {
        assert_eq!(
            list(&pl.id, "processors"),
            pl.processors,
            "processor order of {}",
            pl.id
        );
    }
    assert_eq!(list("traces", "exporters"), ["routing/errors"]);
    assert_eq!(list("traces/errors", "receivers"), ["routing/errors"]);
    assert_eq!(
        list("traces/errors", "exporters"),
        ["otlp_http/otel_test.datadog"]
    );
    assert_eq!(
        list("logs", "exporters"),
        [
            "otlp_http/otel_test.otlp_http/backend",
            "otlp_http/otel_test.debug"
        ]
    );
    assert_eq!(
        resolved["connectors"]["routing/errors"]["table"][0]["condition"],
        Value::from("status.code == STATUS_CODE_ERROR")
    );
}

#[tokio::test]
async fn capture_exporter_settings_take_effect() {
    let c = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), SYNC_CONFIG);
    let cap = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &cap, CaptureProtocol::Http)
        .await
        .unwrap();

    let resolved = print_config(c, &prepared.config_path).await;
    let e = &resolved["exporters"]["otlp_http/otel_test.otlp_http/backend"];
    assert_eq!(
        e["endpoint"],
        Value::from(format!("{}/otlp_http_backend", cap.http_base_url()))
    );
    assert_eq!(
        e["sending_queue"],
        Value::Null,
        "queue must be disabled: {e:?}"
    );
    assert_eq!(e["retry_on_failure"]["enabled"], Value::from(false));
    assert_eq!(e["compression"], Value::from("none"));
    assert_eq!(e["encoding"], Value::from("proto"));
    let level = resolved["service"]["telemetry"]["metrics"]["level"]
        .as_str()
        .unwrap();
    assert!(level.eq_ignore_ascii_case("none"), "{level}");
}

async fn inject(prepared: &Prepared, signal: Signal, fixture: &str) -> reqwest::Response {
    let port = prepared.rewritten.injection_for(signal, None).unwrap().port;
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/{signal}"))
        .header("content-type", "application/json")
        .body(std::fs::read(repo(fixture)).unwrap())
        .send()
        .await
        .unwrap()
}

async fn observe(
    mut window: Window,
    settle: Duration,
    done: impl Fn(&[Captured]) -> bool,
) -> Vec<Captured> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !done(&window.snapshot()) {
        if tokio::time::timeout_at(deadline, window.changed())
            .await
            .is_err()
        {
            break;
        }
    }
    tokio::time::sleep(settle).await;
    window.close()
}

fn summary(items: &[Captured]) -> Vec<(String, usize)> {
    items
        .iter()
        .map(|c| (c.exporter.clone(), c.payload.item_count()))
        .collect()
}

fn item_attributes(p: &Payload) -> Vec<&Vec<KeyValue>> {
    match p {
        Payload::Traces(r) => r
            .resource_spans
            .iter()
            .flat_map(|x| &x.scope_spans)
            .flat_map(|x| &x.spans)
            .map(|s| &s.attributes)
            .collect(),
        Payload::Logs(r) => r
            .resource_logs
            .iter()
            .flat_map(|x| &x.scope_logs)
            .flat_map(|x| &x.log_records)
            .map(|l| &l.attributes)
            .collect(),
        Payload::Metrics(_) => unimplemented!("not needed by these tests"),
    }
}

fn has_attr(p: &Payload, key: &str) -> bool {
    item_attributes(p)
        .iter()
        .any(|attrs| attrs.iter().any(|kv| kv.key == key))
}

#[tokio::test]
async fn captures_processed_telemetry_under_exporter_identity() {
    let c = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), SYNC_CONFIG);
    let cap = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &cap, CaptureProtocol::Http)
        .await
        .unwrap();
    let mut running = prepared
        .start(c, DEFAULT_STARTUP_TIMEOUT)
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let window = cap.store().open_window("login span").unwrap();
    let resp = inject(&prepared, Signal::Traces, "fixtures/traces/login-span.json").await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    let items = window.close();

    assert_eq!(summary(&items), [("otlp_http/backend".to_owned(), 1)]);
    assert!(
        !has_attr(&items[0].payload, "password"),
        "processor must have removed password"
    );
    assert!(has_attr(&items[0].payload, "user.name"));
    assert!(cap.store().take_unattributed().is_empty());
    assert!(cap.store().take_rejected().is_empty());

    assert!(
        running.exit_status().unwrap().is_none(),
        "Collector must still be running"
    );
    assert!(
        running
            .logs()
            .iter()
            .any(|l| l.contains("Everything is ready"))
    );
    let stopped = running.stop(SHUTDOWN_GRACE).await.unwrap();
    assert!(
        stopped.check().is_ok(),
        "graceful shutdown should be clean, got {stopped:?}"
    );
    cap.shutdown().await;
}

#[tokio::test]
async fn capture_outage_is_visible_to_injection() {
    let c = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), SYNC_CONFIG);
    let cap = CaptureServer::start().await.unwrap();
    let base = cap.http_base_url();
    let prepared = session::prepare(&config, c, &cap, CaptureProtocol::Http)
        .await
        .unwrap();
    let running = prepared.start(c, DEFAULT_STARTUP_TIMEOUT).await.unwrap();
    cap.shutdown().await;

    let resp = inject(&prepared, Signal::Traces, "fixtures/traces/login-span.json").await;
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(status.is_server_error(), "{status} {body}");
    assert!(
        body.contains(&format!("{base}/otlp_http_backend/v1/traces")),
        "{body}"
    );
    running.stop(SHUTDOWN_GRACE).await.unwrap();
}

async fn multi_exporter_routing(protocol: CaptureProtocol) {
    let c = collector().await;
    let config =
        CollectorConfig::load(&repo("tests/scenarios/multi-exporter/collector.yaml")).unwrap();
    let cap = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &cap, protocol)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let running = prepared
        .start(c, DEFAULT_STARTUP_TIMEOUT)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let settle = Duration::from_millis(500);
    let logs_rx = prepared
        .rewritten
        .injection_for(Signal::Logs, Some("otlp"))
        .unwrap()
        .port;

    let w = cap.store().open_window("error span").unwrap();
    assert!(
        inject(&prepared, Signal::Traces, "fixtures/traces/error-span.json")
            .await
            .status()
            .is_success()
    );
    let items = observe(w, settle, |i| !i.is_empty()).await;
    assert_eq!(summary(&items), [("datadog".to_owned(), 1)], "{protocol:?}");

    let w = cap.store().open_window("app span").unwrap();
    assert!(
        inject(&prepared, Signal::Traces, "fixtures/traces/app-span.json")
            .await
            .status()
            .is_success()
    );
    let items = observe(w, settle, |i| !i.is_empty()).await;
    assert_eq!(
        summary(&items),
        [("otlp_http/backend".to_owned(), 1)],
        "{protocol:?}"
    );

    let w = cap.store().open_window("healthz").unwrap();
    assert!(
        inject(
            &prepared,
            Signal::Traces,
            "fixtures/traces/healthz-span.json"
        )
        .await
        .status()
        .is_success()
    );
    let items = observe(w, Duration::from_secs(1), |_| true).await;
    assert!(items.is_empty(), "{protocol:?}: {:?}", summary(&items));

    let w = cap.store().open_window("login log").unwrap();
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{logs_rx}/v1/logs"))
        .header("content-type", "application/json")
        .body(std::fs::read(repo("fixtures/logs/login-log.json")).unwrap())
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let items = observe(w, settle, |i| i.len() >= 2).await;
    let mut got = summary(&items);
    got.sort();
    assert_eq!(
        got,
        [("debug".to_owned(), 1), ("otlp_http/backend".to_owned(), 1)],
        "{protocol:?}"
    );
    for item in &items {
        assert!(
            !has_attr(&item.payload, "password"),
            "{} received a password",
            item.exporter
        );
        let expected = match protocol {
            CaptureProtocol::Http => otel_test_capture::Protocol::HttpProtobuf,
            CaptureProtocol::Grpc => otel_test_capture::Protocol::Grpc,
        };
        assert_eq!(item.protocol, expected, "delivered over the wrong protocol");
    }

    assert!(
        cap.store().take_unattributed().is_empty(),
        "nothing may arrive outside a window"
    );
    assert!(cap.store().take_rejected().is_empty());
    running.stop(SHUTDOWN_GRACE).await.unwrap();
    cap.shutdown().await;
}

#[tokio::test]
async fn multi_exporter_routing_over_http() {
    multi_exporter_routing(CaptureProtocol::Http).await;
}

#[tokio::test]
async fn multi_exporter_routing_over_grpc() {
    multi_exporter_routing(CaptureProtocol::Grpc).await;
}

#[tokio::test]
async fn grpc_capture_exporter_settings_take_effect() {
    let c = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), SYNC_CONFIG);
    let cap = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &cap, CaptureProtocol::Grpc)
        .await
        .unwrap();
    let resolved = print_config(c, &prepared.config_path).await;
    let e = &resolved["exporters"]["otlp_grpc/otel_test.otlp_http/backend"];
    assert_eq!(e["endpoint"], Value::from(cap.grpc_endpoint()));
    assert_eq!(e["tls"]["insecure"], Value::from(true));
    assert_eq!(e["sending_queue"], Value::Null, "{e:?}");
    assert_eq!(e["retry_on_failure"]["enabled"], Value::from(false));
    assert_eq!(e["compression"], Value::from("none"));
    let headers = e["headers"].as_sequence().expect("headers list");
    assert!(
        headers
            .iter()
            .any(|h| h["name"].as_str() == Some(otel_test_capture::EXPORTER_HEADER)),
        "{headers:?}"
    );
}

#[tokio::test]
async fn invalid_processor_config_is_rejected_by_validate() {
    let c = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &SYNC_CONFIG.replace("actions:", "actons:"));
    let cap = CaptureServer::start().await.unwrap();
    let err = session::prepare(&config, c, &cap, CaptureProtocol::Http)
        .await
        .unwrap_err();
    let PrepareError::Collector(CollectorError::Invalid { output }) = &err else {
        panic!("expected validation failure, got {err}");
    };
    assert!(output.contains("actons"), "{output}");
}

#[tokio::test]
async fn startup_failure_is_reported_with_logs() {
    let c = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), SYNC_CONFIG);
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = blocker.local_addr().unwrap().port();
    let options = RewriteOptions {
        capture: CaptureTarget::Http {
            base_url: "http://127.0.0.1:9".into(),
            exporter_type: c.otlp_http_exporter().unwrap().into(),
        },
        health_check: false,
    };
    let rewritten = rewrite::rewrite(&config, &options, || Ok(taken)).unwrap();
    let path = dir.path().join("rewritten.yaml");
    std::fs::write(&path, &rewritten.yaml).unwrap();

    let probe = ReadinessProbe {
        tcp_ports: vec![taken],
        health_check_port: Some(otel_test_core::collector::free_port().unwrap()),
        timeout: Duration::from_secs(20),
    };
    let err = c.start(&path, dir.path(), &probe).await.unwrap_err();
    let CollectorError::Exited { status, logs } = &err else {
        panic!("expected the Collector to exit, got {err}");
    };
    assert!(!status.success());
    assert!(
        logs.iter()
            .any(|l| l.contains(&format!("127.0.0.1:{taken}: bind:"))),
        "{logs:#?}"
    );
    assert!(err.to_string().contains("Collector output"), "{err}");
    drop(blocker);
}

#[tokio::test]
async fn readiness_timeout_kills_collector() {
    let c = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), SYNC_CONFIG);
    let cap = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &cap, CaptureProtocol::Http)
        .await
        .unwrap();
    let never = otel_test_core::collector::free_port().unwrap();
    let probe = ReadinessProbe {
        tcp_ports: vec![never],
        health_check_port: None,
        timeout: Duration::from_millis(1500),
    };
    let started = std::time::Instant::now();
    let err = c
        .start(&prepared.config_path, &prepared.cwd, &probe)
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    let CollectorError::NotReady { detail, logs, .. } = &err else {
        panic!("expected NotReady, got {err}");
    };
    assert!(
        detail.contains(&format!("receiver port {never}")),
        "{detail}"
    );
    assert!(
        !logs.is_empty(),
        "logs from the started Collector should be included"
    );

    let real = prepared.rewritten.injections[0].port;
    let mut freed = false;
    for _ in 0..50 {
        if std::net::TcpListener::bind(("127.0.0.1", real)).is_ok() {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(freed, "port {real} still held: Collector was not killed");
}

#[cfg(unix)]
#[tokio::test]
async fn crash_after_startup_is_detected() {
    let c = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), SYNC_CONFIG);
    let cap = CaptureServer::start().await.unwrap();
    let prepared = session::prepare(&config, c, &cap, CaptureProtocol::Http)
        .await
        .unwrap();
    let mut running = prepared.start(c, DEFAULT_STARTUP_TIMEOUT).await.unwrap();
    let pid = running.pid().unwrap().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-9", &pid])
            .status()
            .unwrap()
            .success()
    );

    let mut exited = None;
    for _ in 0..50 {
        if let Some(s) = running.exit_status().unwrap() {
            exited = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let exited = exited.expect("crash was not detected");
    assert!(!exited.success());
    assert!(running.stop(SHUTDOWN_GRACE).await.unwrap().check().is_err());
}
