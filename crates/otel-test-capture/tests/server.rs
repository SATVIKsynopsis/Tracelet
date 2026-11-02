use std::io::Write;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use otel_test_capture::{CaptureServer, EXPORTER_HEADER, Payload, Protocol};
use prost::Message;

fn fixture(path: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../fixtures/{path}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn traces_fixture() -> ExportTraceServiceRequest {
    serde_json::from_str(&fixture("traces/error-span.json")).unwrap()
}

async fn server() -> CaptureServer {
    let s = CaptureServer::start().await.unwrap();
    s.store().register("datadog", "datadog");
    s.store().register("otlp_http_backend", "otlp_http/backend");
    s
}

async fn post(
    s: &CaptureServer,
    path: &str,
    content_type: &str,
    encoding: Option<&str>,
    body: Vec<u8>,
) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{}{path}", s.http_base_url()))
        .header("content-type", content_type)
        .body(body);
    if let Some(e) = encoding {
        req = req.header("content-encoding", e);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn http_protobuf_export_is_recorded_under_original_exporter() {
    let s = server().await;
    let w = s.store().open_window("t").unwrap();
    let req = traces_fixture();
    let resp = post(
        &s,
        "/otlp_http_backend/v1/traces",
        "application/x-protobuf",
        None,
        req.encode_to_vec(),
    )
    .await;

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/x-protobuf");
    let body = resp.bytes().await.unwrap();
    assert_eq!(
        ExportTraceServiceResponse::decode(body).unwrap(),
        ExportTraceServiceResponse::default()
    );

    let items = w.close();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].exporter, "otlp_http/backend");
    assert_eq!(items[0].protocol, Protocol::HttpProtobuf);
    assert_eq!(items[0].payload, Payload::Traces(req));
    s.shutdown().await;
}

#[tokio::test]
async fn http_json_and_gzip_are_accepted() {
    let s = server().await;
    let w = s.store().open_window("t").unwrap();

    let resp = post(
        &s,
        "/datadog/v1/logs",
        "application/json; charset=utf-8",
        None,
        fixture("logs/login-log.json").into(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(resp.text().await.unwrap(), "{}");

    let metrics: ExportMetricsServiceRequest =
        serde_json::from_str(&fixture("metrics/http-requests.json")).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&metrics.encode_to_vec()).unwrap();
    let resp = post(
        &s,
        "/datadog/v1/metrics",
        "application/x-protobuf",
        Some("gzip"),
        gz.finish().unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let items = w.close();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].protocol, Protocol::HttpJson);
    assert_eq!(items[0].payload.item_count(), 1);
    let expected_logs: ExportLogsServiceRequest =
        serde_json::from_str(&fixture("logs/login-log.json")).unwrap();
    assert_eq!(items[0].payload, Payload::Logs(expected_logs));
    assert_eq!(items[1].payload, Payload::Metrics(metrics));
    assert_eq!(items[1].payload.item_count(), 2);
}

type ErrorCase = (
    &'static str,
    &'static str,
    Option<&'static str>,
    Vec<u8>,
    u16,
    &'static str,
);

#[tokio::test]
async fn http_errors_are_rejected_and_reported() {
    let s = server().await;
    let w = s.store().open_window("t").unwrap();
    let proto = traces_fixture().encode_to_vec();

    let cases: Vec<ErrorCase> = vec![
        (
            "/unknown/v1/traces",
            "application/x-protobuf",
            None,
            proto.clone(),
            404,
            "no exporter registered for path /unknown/v1/traces",
        ),
        (
            "/datadog/v1/profiles",
            "application/x-protobuf",
            None,
            proto.clone(),
            404,
            "unknown OTLP signal path",
        ),
        (
            "/datadog/v1/traces",
            "text/plain",
            None,
            proto.clone(),
            415,
            "unsupported content type \"text/plain\"",
        ),
        (
            "/datadog/v1/traces",
            "application/x-protobuf",
            Some("br"),
            proto.clone(),
            415,
            "unsupported content encoding \"br\"",
        ),
        (
            "/datadog/v1/traces",
            "application/x-protobuf",
            Some("gzip"),
            proto.clone(),
            400,
            "invalid gzip body",
        ),
        (
            "/datadog/v1/traces",
            "application/x-protobuf",
            None,
            vec![0xff, 0xff, 0xff],
            400,
            "undecodable traces request from exporter datadog",
        ),
        (
            "/datadog/v1/traces",
            "application/json",
            None,
            b"{\"resourceSpans\": 3}".to_vec(),
            400,
            "undecodable traces request",
        ),
    ];
    for (path, ct, enc, body, status, message) in &cases {
        let resp = post(&s, path, ct, *enc, body.clone()).await;
        assert_eq!(resp.status().as_u16(), *status, "{path} {ct} {enc:?}");
        let text = resp.text().await.unwrap();
        assert!(text.contains(message), "{path}: {text:?} lacks {message:?}");
    }
    let resp = reqwest::get(format!("{}/datadog/v1/traces", s.http_base_url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 405, "GET is not an OTLP export");

    assert!(
        w.close().is_empty(),
        "rejected requests must not count as captured telemetry"
    );
    let rejected = s.store().take_rejected();
    assert_eq!(rejected.len(), cases.len());
}

#[tokio::test]
async fn grpc_exports_use_exporter_header() {
    let s = server().await;
    let w = s.store().open_window("t").unwrap();
    let url = format!("http://{}", s.grpc_endpoint());

    let mut req = tonic::Request::new(traces_fixture());
    req.metadata_mut()
        .insert(EXPORTER_HEADER, "datadog".parse().unwrap());
    TraceServiceClient::connect(url.clone())
        .await
        .unwrap()
        .export(req)
        .await
        .unwrap();

    let mut req = tonic::Request::new(
        serde_json::from_str::<ExportLogsServiceRequest>(&fixture("logs/login-log.json")).unwrap(),
    );
    req.metadata_mut()
        .insert(EXPORTER_HEADER, "otlp_http_backend".parse().unwrap());
    LogsServiceClient::connect(url.clone())
        .await
        .unwrap()
        .export(req)
        .await
        .unwrap();

    let mut req = tonic::Request::new(ExportMetricsServiceRequest::default());
    req.metadata_mut()
        .insert(EXPORTER_HEADER, "datadog".parse().unwrap());
    MetricsServiceClient::connect(url.clone())
        .await
        .unwrap()
        .export(req)
        .await
        .unwrap();

    let items = w.close();
    let got: Vec<(&str, Protocol, usize)> = items
        .iter()
        .map(|c| (c.exporter.as_str(), c.protocol, c.payload.item_count()))
        .collect();
    assert_eq!(
        got,
        [
            ("datadog", Protocol::Grpc, 1),
            ("otlp_http/backend", Protocol::Grpc, 1),
            ("datadog", Protocol::Grpc, 0)
        ]
    );
    assert_eq!(items[0].payload, Payload::Traces(traces_fixture()));
}

#[tokio::test]
async fn grpc_without_known_exporter_is_rejected() {
    let s = server().await;
    let url = format!("http://{}", s.grpc_endpoint());
    let mut client = TraceServiceClient::connect(url).await.unwrap();

    let err = client.export(traces_fixture()).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains(EXPORTER_HEADER));

    let mut req = tonic::Request::new(traces_fixture());
    req.metadata_mut()
        .insert(EXPORTER_HEADER, "kafka".parse().unwrap());
    let err = client.export(req).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    assert_eq!(s.store().take_rejected().len(), 2);
    assert!(s.store().take_unattributed().is_empty());
}

#[tokio::test]
async fn data_outside_windows_is_unattributed() {
    let s = server().await;
    let body = traces_fixture().encode_to_vec();
    post(
        &s,
        "/datadog/v1/traces",
        "application/x-protobuf",
        None,
        body.clone(),
    )
    .await;
    let w = s.store().open_window("t").unwrap();
    post(
        &s,
        "/datadog/v1/traces",
        "application/x-protobuf",
        None,
        body.clone(),
    )
    .await;
    let in_window = w.close();
    post(
        &s,
        "/datadog/v1/traces",
        "application/x-protobuf",
        None,
        body,
    )
    .await;

    assert_eq!(in_window.len(), 1);
    assert_eq!(s.store().take_unattributed().len(), 2);
}

#[tokio::test]
async fn shutdown_stops_listening() {
    let s = server().await;
    let http = s.http_base_url();
    let grpc = s.grpc_endpoint();
    s.shutdown().await;
    assert!(
        reqwest::Client::new()
            .post(format!("{http}/datadog/v1/traces"))
            .send()
            .await
            .is_err()
    );
    assert!(tokio::net::TcpStream::connect(grpc).await.is_err());
}
