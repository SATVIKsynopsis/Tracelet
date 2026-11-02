use std::io::Read;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use prost::Message;

use crate::MAX_REQUEST_BYTES;
use crate::store::{Payload, Protocol, Signal, Store};

const PROTOBUF: &str = "application/x-protobuf";
const JSON: &str = "application/json";

pub(crate) fn router(store: Store) -> Router {
    Router::new()
        .route("/{exporter}/v1/{signal}", post(export))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(store)
}

async fn export(
    State(store): State<Store>,
    Path((slug, signal)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(signal) = Signal::from_path_segment(&signal) else {
        return reject(
            &store,
            StatusCode::NOT_FOUND,
            format!("unknown OTLP signal path /{slug}/v1/{signal}"),
        );
    };
    let Some(exporter) = store.exporter_for(&slug) else {
        return reject(
            &store,
            StatusCode::NOT_FOUND,
            format!("no exporter registered for path /{slug}/v1/{signal}"),
        );
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    let protocol = match content_type.as_str() {
        PROTOBUF => Protocol::HttpProtobuf,
        JSON => Protocol::HttpJson,
        other => {
            return reject(
                &store,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("unsupported content type {other:?} from exporter {exporter}"),
            );
        }
    };

    let body = match headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
    {
        None | Some("identity") => body.to_vec(),
        Some("gzip") => match gunzip(&body) {
            Ok(b) => b,
            Err(e) => {
                return reject(
                    &store,
                    StatusCode::BAD_REQUEST,
                    format!("invalid gzip body from {exporter}: {e}"),
                );
            }
        },
        Some(other) => {
            return reject(
                &store,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("unsupported content encoding {other:?} from exporter {exporter}"),
            );
        }
    };

    let payload = match decode(signal, protocol, &body) {
        Ok(p) => p,
        Err(e) => {
            return reject(
                &store,
                StatusCode::BAD_REQUEST,
                format!("undecodable {signal} request from exporter {exporter}: {e}"),
            );
        }
    };
    store.record(exporter, protocol, payload);
    success(signal, protocol)
}

fn decode(signal: Signal, protocol: Protocol, body: &[u8]) -> Result<Payload, String> {
    fn proto<M: Message + Default>(b: &[u8]) -> Result<M, String> {
        M::decode(b).map_err(|e| e.to_string())
    }
    fn json<M: serde::de::DeserializeOwned>(b: &[u8]) -> Result<M, String> {
        serde_json::from_slice(b).map_err(|e| e.to_string())
    }
    let is_json = protocol == Protocol::HttpJson;
    Ok(match (signal, is_json) {
        (Signal::Traces, false) => Payload::Traces(proto::<ExportTraceServiceRequest>(body)?),
        (Signal::Traces, true) => Payload::Traces(json::<ExportTraceServiceRequest>(body)?),
        (Signal::Metrics, false) => Payload::Metrics(proto::<ExportMetricsServiceRequest>(body)?),
        (Signal::Metrics, true) => Payload::Metrics(json::<ExportMetricsServiceRequest>(body)?),
        (Signal::Logs, false) => Payload::Logs(proto::<ExportLogsServiceRequest>(body)?),
        (Signal::Logs, true) => Payload::Logs(json::<ExportLogsServiceRequest>(body)?),
    })
}

fn success(signal: Signal, protocol: Protocol) -> Response {
    if protocol == Protocol::HttpJson {
        return ([(header::CONTENT_TYPE, JSON)], "{}").into_response();
    }
    let body = match signal {
        Signal::Traces => ExportTraceServiceResponse::default().encode_to_vec(),
        Signal::Metrics => ExportMetricsServiceResponse::default().encode_to_vec(),
        Signal::Logs => ExportLogsServiceResponse::default().encode_to_vec(),
    };
    ([(header::CONTENT_TYPE, PROTOBUF)], body).into_response()
}

fn reject(store: &Store, status: StatusCode, reason: String) -> Response {
    store.reject(reason.clone());
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        reason,
    )
        .into_response()
}

fn gunzip(body: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > MAX_REQUEST_BYTES {
        return Err(std::io::Error::other(
            "decompressed body exceeds size limit",
        ));
    }
    Ok(out)
}
