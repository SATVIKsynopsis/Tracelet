use std::path::{Path, PathBuf};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use otel_test_capture::Payload;

use crate::spec::Signal;

#[derive(Debug, Clone)]
pub struct Fixture {
    pub path: PathBuf,
    pub signal: Signal,
    pub item_count: usize,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("failed to read fixture {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid fixture {}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

const KEYS: [(&str, Signal); 3] = [
    ("resourceSpans", Signal::Traces),
    ("resourceMetrics", Signal::Metrics),
    ("resourceLogs", Signal::Logs),
];

impl Fixture {
    pub fn load(path: &Path, declared: Option<Signal>) -> Result<Self, FixtureError> {
        let body = std::fs::read(path).map_err(|source| FixtureError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(path, body, declared)
    }

    pub fn parse(
        path: &Path,
        body: Vec<u8>,
        declared: Option<Signal>,
    ) -> Result<Self, FixtureError> {
        let invalid = |message: String| FixtureError::Invalid {
            path: path.to_path_buf(),
            message,
        };
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| invalid(format!("not valid JSON: {e}")))?;
        let Some(object) = value.as_object() else {
            return Err(invalid(
                "expected a JSON object with `resourceSpans`, `resourceMetrics` or `resourceLogs`"
                    .into(),
            ));
        };
        let present: Vec<(&str, Signal)> = KEYS
            .into_iter()
            .filter(|(k, _)| object.contains_key(*k))
            .collect();
        let signal = match present.as_slice() {
            [(_, s)] => *s,
            [] => {
                let found: Vec<&str> = object.keys().map(String::as_str).collect();
                return Err(invalid(format!(
                    "expected one of `resourceSpans`, `resourceMetrics` or `resourceLogs` at the top level, found {found:?}{}",
                    snake_case_hint(object)
                )));
            }
            _ => {
                let keys: Vec<&str> = present.iter().map(|(k, _)| *k).collect();
                return Err(invalid(format!(
                    "contains several signals ({}); a fixture is a single OTLP export request",
                    keys.join(", ")
                )));
            }
        };
        if let Some(d) = declared
            && d != signal
        {
            return Err(invalid(format!(
                "test declares `signal: {d}` but the fixture contains {signal}"
            )));
        }

        let decoded = match signal {
            Signal::Traces => {
                serde_json::from_value::<ExportTraceServiceRequest>(value).map(Payload::Traces)
            }
            Signal::Metrics => {
                serde_json::from_value::<ExportMetricsServiceRequest>(value).map(Payload::Metrics)
            }
            Signal::Logs => {
                serde_json::from_value::<ExportLogsServiceRequest>(value).map(Payload::Logs)
            }
        }
        .map_err(|e| invalid(format!("not a valid OTLP/JSON {signal} request: {e}")))?;
        let item_count = decoded.item_count();
        if item_count == 0 {
            return Err(invalid(format!("contains no {}", signal_items(signal, 2))));
        }
        Ok(Fixture {
            path: path.to_path_buf(),
            signal,
            item_count,
            body,
        })
    }
}

fn snake_case_hint(object: &serde_json::Map<String, serde_json::Value>) -> &'static str {
    if ["resource_spans", "resource_metrics", "resource_logs"]
        .iter()
        .any(|k| object.contains_key(*k))
    {
        " (OTLP/JSON uses lowerCamelCase field names, e.g. `resourceSpans`)"
    } else {
        ""
    }
}

pub fn signal_items(signal: Signal, n: usize) -> String {
    let (one, many) = match signal {
        Signal::Traces => ("span", "spans"),
        Signal::Logs => ("log record", "log records"),
        Signal::Metrics => ("data point", "data points"),
    };
    if n == 1 {
        one.to_owned()
    } else {
        many.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(path: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }

    fn parse(json: &str, declared: Option<Signal>) -> Result<Fixture, FixtureError> {
        Fixture::parse(Path::new("f.json"), json.as_bytes().to_vec(), declared)
    }

    fn err(json: &str) -> String {
        parse(json, None).unwrap_err().to_string()
    }

    #[test]
    fn loads_every_bundled_fixture() {
        let cases = [
            ("fixtures/traces/app-span.json", Signal::Traces, 1),
            ("fixtures/traces/error-span.json", Signal::Traces, 1),
            ("fixtures/traces/healthz-span.json", Signal::Traces, 1),
            ("fixtures/traces/login-span.json", Signal::Traces, 1),
            ("fixtures/logs/login-log.json", Signal::Logs, 1),
            ("fixtures/metrics/http-requests.json", Signal::Metrics, 2),
        ];
        for (path, signal, items) in cases {
            let f = Fixture::load(&repo(path), None).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!((f.signal, f.item_count), (signal, items), "{path}");
            assert_eq!(
                f.body,
                std::fs::read(repo(path)).unwrap(),
                "body must be injected verbatim"
            );
        }
    }

    #[test]
    fn declared_signal_must_match() {
        let f = Fixture::load(&repo("fixtures/logs/login-log.json"), Some(Signal::Logs)).unwrap();
        assert_eq!(f.signal, Signal::Logs);
        let e =
            Fixture::load(&repo("fixtures/logs/login-log.json"), Some(Signal::Traces)).unwrap_err();
        assert!(
            e.to_string()
                .contains("declares `signal: traces` but the fixture contains logs"),
            "{e}"
        );
    }

    #[test]
    fn rejects_invalid_fixtures() {
        assert!(err("{").contains("not valid JSON"));
        assert!(err("[]").contains("expected a JSON object"));
        assert!(err(r#"{"spans": []}"#).contains("found [\"spans\"]"));
        assert!(err(r#"{"resource_spans": []}"#).contains("lowerCamelCase"));
        assert!(err(r#"{"resourceSpans": [], "resourceLogs": []}"#).contains("several signals"));
        assert!(err(r#"{"resourceSpans": []}"#).contains("contains no spans"));
        let e = err(
            r#"{"resourceSpans": [{"scopeSpans": [{"spans": [{"traceId": "not-hex", "name": "x"}]}]}]}"#,
        );
        assert!(e.contains("not a valid OTLP/JSON traces request"), "{e}");
        let e = err(
            r#"{"resourceLogs": [{"scopeLogs": [{"logRecords": [{"severityNumber": "loud"}]}]}]}"#,
        );
        assert!(e.contains("not a valid OTLP/JSON logs request"), "{e}");
        assert!(matches!(
            Fixture::load(Path::new("/nope.json"), None),
            Err(FixtureError::Read { .. })
        ));
    }

    #[test]
    fn item_nouns() {
        assert_eq!(signal_items(Signal::Traces, 1), "span");
        assert_eq!(signal_items(Signal::Traces, 0), "spans");
        assert_eq!(signal_items(Signal::Logs, 2), "log records");
        assert_eq!(signal_items(Signal::Metrics, 1), "data point");
    }
}
