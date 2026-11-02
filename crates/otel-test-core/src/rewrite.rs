use std::collections::{BTreeSet, HashSet};

use serde_yaml::{Mapping, Value};

use otel_test_capture::EXPORTER_HEADER;

use crate::config::{CollectorConfig, split_id};
use crate::spec::Signal;

pub const NAME_PREFIX: &str = "otel_test.";
const HEALTH_CHECK_ID: &str = "health_check/otel_test";

#[derive(Debug, Clone)]
pub enum CaptureTarget {
    Http {
        base_url: String,
        exporter_type: String,
    },
    Grpc {
        endpoint: String,
        exporter_type: String,
    },
}

#[derive(Debug, Clone)]
pub struct RewriteOptions {
    pub capture: CaptureTarget,
    pub health_check: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    pub original: String,
    pub component: String,
    pub port: u16,
    pub signals: BTreeSet<Signal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureExporter {
    pub original: String,
    pub component: String,
    pub slug: String,
    pub signals: BTreeSet<Signal>,
}

#[derive(Debug, Clone)]
pub struct Rewritten {
    pub yaml: String,
    pub injections: Vec<Injection>,
    pub exporters: Vec<CaptureExporter>,
    pub health_check_port: Option<u16>,
    pub notes: Vec<String>,
    pub startup_hints: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RewriteError {
    #[error("failed to allocate a local port: {0}")]
    Port(#[source] std::io::Error),
    #[error("{0}")]
    Conflict(String),
    #[error("failed to serialize rewritten configuration: {0}")]
    Serialize(#[source] serde_yaml::Error),
}

impl Rewritten {
    pub fn injection_for(
        &self,
        signal: Signal,
        receiver: Option<&str>,
    ) -> Result<&Injection, String> {
        let candidates: Vec<&Injection> = self
            .injections
            .iter()
            .filter(|i| i.signals.contains(&signal))
            .collect();
        let names = || {
            candidates
                .iter()
                .map(|i| i.original.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match receiver {
            Some(r) => {
                if let Some(i) = candidates.iter().find(|i| i.original == r) {
                    return Ok(i);
                }
                if self.injections.iter().any(|i| i.original == r) {
                    Err(format!(
                        "receiver {r:?} does not feed any {signal} pipeline (candidates: {})",
                        names()
                    ))
                } else {
                    Err(format!(
                        "receiver {r:?} is not used by any pipeline in the Collector configuration (candidates for {signal}: {})",
                        names()
                    ))
                }
            }
            None => match candidates.as_slice() {
                [only] => Ok(only),
                [] => Err(format!(
                    "the Collector configuration has no {signal} pipeline"
                )),
                _ => Err(format!(
                    "several receivers feed {signal} pipelines ({}); set `receiver:` on the test or in `defaults`",
                    names()
                )),
            },
        }
    }

    pub fn exporter(&self, original: &str) -> Option<&CaptureExporter> {
        self.exporters.iter().find(|e| e.original == original)
    }
}

pub fn rewrite(
    config: &CollectorConfig,
    options: &RewriteOptions,
    mut alloc_port: impl FnMut() -> std::io::Result<u16>,
) -> Result<Rewritten, RewriteError> {
    let mut notes = Vec::new();
    let mut root = config.root.clone();

    let exporter_type = match &options.capture {
        CaptureTarget::Http { exporter_type, .. } | CaptureTarget::Grpc { exporter_type, .. } => {
            exporter_type
        }
    };

    let mut injections: Vec<Injection> = Vec::new();
    let mut exporters: Vec<CaptureExporter> = Vec::new();
    let mut slugs = HashSet::new();
    for pl in &config.pipelines {
        for r in pl.receivers.iter().filter(|r| !config.is_connector(r)) {
            if let Some(i) = injections.iter_mut().find(|i| &i.original == r) {
                i.signals.insert(pl.signal);
            } else {
                injections.push(Injection {
                    original: r.clone(),
                    component: format!("otlp/{NAME_PREFIX}{r}"),
                    port: alloc_port().map_err(RewriteError::Port)?,
                    signals: BTreeSet::from([pl.signal]),
                });
            }
        }
        for e in pl.exporters.iter().filter(|e| !config.is_connector(e)) {
            if let Some(x) = exporters.iter_mut().find(|x| &x.original == e) {
                x.signals.insert(pl.signal);
            } else {
                let slug = unique_slug(e, &mut slugs);
                exporters.push(CaptureExporter {
                    original: e.clone(),
                    component: format!("{}/{NAME_PREFIX}{e}", exporter_type),
                    slug,
                    signals: BTreeSet::from([pl.signal]),
                });
            }
        }
    }

    let mut receivers_out = Mapping::new();
    for i in &injections {
        receivers_out.insert(
            i.component.clone().into(),
            injection_receiver_config(i.port),
        );
        notes.push(format!(
            "receiver {} replaced by {} on 127.0.0.1:{} ({})",
            i.original,
            i.component,
            i.port,
            join(&i.signals)
        ));
    }
    for r in config
        .receivers
        .iter()
        .filter(|r| !injections.iter().any(|i| &i.original == *r))
    {
        notes.push(format!("receiver {r} removed: not used by any pipeline"));
    }
    root.insert("receivers".into(), Value::Mapping(receivers_out));

    let mut exporters_out = Mapping::new();
    for x in &exporters {
        exporters_out.insert(
            x.component.clone().into(),
            capture_exporter_config(&options.capture, &x.slug),
        );
        notes.push(format!(
            "exporter {} replaced by {} ({})",
            x.original,
            x.component,
            join(&x.signals)
        ));
    }
    for e in config
        .exporters
        .iter()
        .filter(|e| !exporters.iter().any(|x| &x.original == *e))
    {
        notes.push(format!("exporter {e} removed: not used by any pipeline"));
    }
    root.insert("exporters".into(), Value::Mapping(exporters_out));

    let pipelines = root
        .get_mut("service")
        .and_then(Value::as_mapping_mut)
        .and_then(|s| s.get_mut("pipelines"))
        .and_then(Value::as_mapping_mut)
        .expect("CollectorConfig guarantees service.pipelines is a mapping");
    for pl in &config.pipelines {
        let m = pipelines
            .get_mut(pl.id.as_str())
            .and_then(Value::as_mapping_mut)
            .expect("pipeline exists in the source document");
        let receivers: Vec<Value> = pl
            .receivers
            .iter()
            .map(|r| match injections.iter().find(|i| &i.original == r) {
                Some(i) => i.component.clone().into(),
                None => r.clone().into(),
            })
            .collect();
        let exporters_list: Vec<Value> = pl
            .exporters
            .iter()
            .map(|e| match exporters.iter().find(|x| &x.original == e) {
                Some(x) => x.component.clone().into(),
                None => e.clone().into(),
            })
            .collect();
        m.insert("receivers".into(), Value::Sequence(receivers));
        m.insert("exporters".into(), Value::Sequence(exporters_list));
    }

    let kept_extensions = referenced_extensions(config);
    let mut extensions_out = Mapping::new();
    if let Some(Value::Mapping(m)) = config.root.get("extensions") {
        for (k, v) in m {
            let id = k.as_str().expect("validated by CollectorConfig");
            if kept_extensions.contains(id) {
                extensions_out.insert(k.clone(), v.clone());
            } else {
                notes.push(format!(
                    "extension {id} removed: not referenced by any processor or connector"
                ));
            }
        }
    }
    let mut service_extensions: Vec<Value> = config
        .service_extensions
        .iter()
        .filter(|e| kept_extensions.contains(e.as_str()))
        .map(|e| e.clone().into())
        .collect();

    let health_check_port = if options.health_check {
        if extensions_out.contains_key(HEALTH_CHECK_ID) {
            return Err(RewriteError::Conflict(format!(
                "extension ID {HEALTH_CHECK_ID} is reserved by otel-test; rename it in the Collector configuration"
            )));
        }
        let port = alloc_port().map_err(RewriteError::Port)?;
        let mut hc = Mapping::new();
        hc.insert("endpoint".into(), format!("127.0.0.1:{port}").into());
        extensions_out.insert(HEALTH_CHECK_ID.into(), Value::Mapping(hc));
        service_extensions.push(HEALTH_CHECK_ID.into());
        notes.push(format!(
            "extension {HEALTH_CHECK_ID} added on 127.0.0.1:{port} for readiness checks"
        ));
        Some(port)
    } else {
        None
    };
    if extensions_out.is_empty() {
        root.remove("extensions");
    } else {
        root.insert("extensions".into(), Value::Mapping(extensions_out));
    }

    let service = root
        .get_mut("service")
        .and_then(Value::as_mapping_mut)
        .expect("validated by CollectorConfig");
    if service_extensions.is_empty() {
        service.remove("extensions");
    } else {
        service.insert("extensions".into(), Value::Sequence(service_extensions));
    }
    let log_level = service
        .get("telemetry")
        .and_then(|t| t.get("logs"))
        .and_then(|l| l.get("level"))
        .and_then(Value::as_str)
        .unwrap_or("info")
        .to_owned();
    if service.contains_key("telemetry") {
        notes.push(format!(
            "service.telemetry replaced: self-metrics disabled, logs at level {log_level} to stderr"
        ));
    }
    service.insert("telemetry".into(), telemetry_config(&log_level));

    let yaml = serde_yaml::to_string(&Value::Mapping(root)).map_err(RewriteError::Serialize)?;
    let header = format!(
        "# Generated by otel-test from {}. Do not edit.\n",
        config.source.display()
    );
    Ok(Rewritten {
        yaml: header + &yaml,
        injections,
        exporters,
        health_check_port,
        notes,
        startup_hints: startup_hints(config),
    })
}

fn startup_hints(config: &CollectorConfig) -> Vec<String> {
    let mut hints = Vec::new();
    let mut seen = HashSet::new();
    for p in config.pipelines.iter().flat_map(|p| &p.processors) {
        if !matches!(split_id(p).0, "k8s_attributes" | "k8sattributes") || !seen.insert(p) {
            continue;
        }
        let passthrough = config
            .component("processors", p)
            .and_then(|c| c.get("passthrough"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !passthrough {
            hints.push(format!(
                "processor {p} needs the Kubernetes API and does not start outside a cluster; test with `passthrough: true` or without it"
            ));
        }
    }
    hints
}

fn injection_receiver_config(port: u16) -> Value {
    serde_yaml::from_str(&format!(
        "protocols:\n  http:\n    endpoint: 127.0.0.1:{port}\n"
    ))
    .expect("static YAML")
}

fn capture_exporter_config(target: &CaptureTarget, slug: &str) -> Value {
    let mut m: Mapping = serde_yaml::from_str(
        "compression: none\ntimeout: 5s\nretry_on_failure:\n  enabled: false\nsending_queue:\n  enabled: false\n",
    )
    .expect("static YAML");
    match target {
        CaptureTarget::Http { base_url, .. } => {
            let endpoint = format!("{}/{slug}", base_url.trim_end_matches('/'));
            m.insert("endpoint".into(), endpoint.into());
            m.insert("encoding".into(), "proto".into());
        }
        CaptureTarget::Grpc { endpoint, .. } => {
            m.insert("endpoint".into(), endpoint.clone().into());
            let mut tls = Mapping::new();
            tls.insert("insecure".into(), true.into());
            m.insert("tls".into(), Value::Mapping(tls));
            let mut headers = Mapping::new();
            headers.insert(EXPORTER_HEADER.into(), slug.into());
            m.insert("headers".into(), Value::Mapping(headers));
        }
    }
    Value::Mapping(m)
}

fn telemetry_config(level: &str) -> Value {
    let mut v: Value = serde_yaml::from_str(
        "metrics:\n  level: none\nlogs:\n  output_paths: [stderr]\n  error_output_paths: [stderr]\n  sampling:\n    enabled: false\n",
    )
    .expect("static YAML");
    v["logs"]
        .as_mapping_mut()
        .expect("static YAML")
        .insert("level".into(), level.into());
    v
}

fn unique_slug(id: &str, taken: &mut HashSet<String>) -> String {
    let base: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut slug = base.clone();
    let mut n = 2;
    while !taken.insert(slug.clone()) {
        slug = format!("{base}-{n}");
        n += 1;
    }
    slug
}

fn referenced_extensions(config: &CollectorConfig) -> HashSet<String> {
    let mut strings = HashSet::new();
    for section in ["processors", "connectors"] {
        if let Some(v) = config.root.get(section) {
            collect_strings(v, &mut strings);
        }
    }
    let mut kept: HashSet<String> = HashSet::new();
    loop {
        let before = kept.len();
        for e in &config.extensions {
            if strings.contains(e.as_str())
                && kept.insert(e.clone())
                && let Some(v) = config.component("extensions", e)
            {
                collect_strings(v, &mut strings);
            }
        }
        if kept.len() == before {
            return kept;
        }
    }
}

fn collect_strings(v: &Value, out: &mut HashSet<String>) {
    match v {
        Value::String(s) => {
            out.insert(s.clone());
        }
        Value::Sequence(s) => s.iter().for_each(|x| collect_strings(x, out)),
        Value::Mapping(m) => m.values().for_each(|x| collect_strings(x, out)),
        Value::Tagged(t) => collect_strings(&t.value, out),
        _ => {}
    }
}

fn join(signals: &BTreeSet<Signal>) -> String {
    signals
        .iter()
        .map(Signal::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn options() -> RewriteOptions {
        RewriteOptions {
            capture: CaptureTarget::Http {
                base_url: "http://127.0.0.1:9000/".into(),
                exporter_type: "otlp_http".into(),
            },
            health_check: true,
        }
    }

    fn run(yaml: &str) -> (Rewritten, Value) {
        let config = CollectorConfig::parse(yaml, Path::new("/src/collector.yaml"))
            .unwrap_or_else(|e| panic!("{e}"));
        let mut next = 20000;
        let out = rewrite(&config, &options(), || {
            next += 1;
            Ok(next)
        })
        .unwrap();
        let value: Value = serde_yaml::from_str(&out.yaml).unwrap();
        (out, value)
    }

    fn strings(v: &Value) -> Vec<&str> {
        v.as_sequence()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect()
    }

    fn keys(v: &Value) -> Vec<&str> {
        v.as_mapping()
            .unwrap()
            .keys()
            .map(|k| k.as_str().unwrap())
            .collect()
    }

    const CONFIG: &str = r#"
receivers:
  otlp:
    protocols: {grpc: {endpoint: 0.0.0.0:4317}}
  filelog/app:
    include: [/var/log/app.log]
  prometheus:
    config: {}
processors:
  filter/healthz:
    traces:
      span: ['attributes["http.route"] == "/healthz"']
  attributes/redact:
    actions: [{key: password, action: delete}]
  batch: {timeout: 1s}
  unused/proc: {}
exporters:
  datadog:
    api: {key: "${env:DD_API_KEY}"}
    auth: {authenticator: basicauth/dd}
  otlp_http/backend:
    endpoint: https://otlp.example.com
  debug: {}
  kafka: {}
connectors:
  routing/errors:
    default_pipelines: [traces/default]
    table:
      - statement: route() where status.code == STATUS_CODE_ERROR
        pipelines: [traces/errors]
extensions:
  basicauth/dd: {client_auth: {username: u, password: p}}
  health_check: {}
  file_storage/tail: {directory: /tmp}
service:
  extensions: [health_check, basicauth/dd, file_storage/tail]
  telemetry:
    logs: {level: debug}
    metrics: {address: 0.0.0.0:8888}
  pipelines:
    traces:
      receivers: [otlp]
      processors: [filter/healthz, attributes/redact, batch]
      exporters: [routing/errors, debug]
    traces/errors:
      receivers: [routing/errors]
      exporters: [datadog]
    traces/default:
      receivers: [routing/errors]
      processors: [batch]
      exporters: [otlp_http/backend]
    logs:
      receivers: [filelog/app, otlp]
      processors: [attributes/redact]
      exporters: [otlp_http/backend, debug]
"#;

    #[test]
    fn replaces_receivers_and_exporters_but_keeps_connectors() {
        let (out, v) = run(CONFIG);
        let pl = &v["service"]["pipelines"];
        assert_eq!(
            keys(pl),
            ["traces", "traces/errors", "traces/default", "logs"]
        );

        assert_eq!(strings(&pl["traces"]["receivers"]), ["otlp/otel_test.otlp"]);
        assert_eq!(
            strings(&pl["traces"]["exporters"]),
            ["routing/errors", "otlp_http/otel_test.debug"]
        );
        assert_eq!(
            strings(&pl["traces/errors"]["receivers"]),
            ["routing/errors"]
        );
        assert_eq!(
            strings(&pl["traces/errors"]["exporters"]),
            ["otlp_http/otel_test.datadog"]
        );
        assert_eq!(
            strings(&pl["traces/default"]["exporters"]),
            ["otlp_http/otel_test.otlp_http/backend"]
        );
        assert_eq!(
            strings(&pl["logs"]["receivers"]),
            ["otlp/otel_test.filelog/app", "otlp/otel_test.otlp"]
        );
        assert_eq!(
            strings(&pl["logs"]["exporters"]),
            [
                "otlp_http/otel_test.otlp_http/backend",
                "otlp_http/otel_test.debug"
            ]
        );

        assert_eq!(
            out.injections,
            vec![
                Injection {
                    original: "otlp".into(),
                    component: "otlp/otel_test.otlp".into(),
                    port: 20001,
                    signals: BTreeSet::from([Signal::Traces, Signal::Logs]),
                },
                Injection {
                    original: "filelog/app".into(),
                    component: "otlp/otel_test.filelog/app".into(),
                    port: 20002,
                    signals: BTreeSet::from([Signal::Logs]),
                },
            ]
        );
        let originals: Vec<_> = out
            .exporters
            .iter()
            .map(|e| (e.original.as_str(), e.slug.as_str()))
            .collect();
        assert_eq!(
            originals,
            [
                ("debug", "debug"),
                ("datadog", "datadog"),
                ("otlp_http/backend", "otlp_http_backend")
            ]
        );
        assert_eq!(
            out.exporter("debug").unwrap().signals,
            BTreeSet::from([Signal::Traces, Signal::Logs])
        );

        assert_eq!(
            keys(&v["receivers"]),
            ["otlp/otel_test.otlp", "otlp/otel_test.filelog/app"]
        );
        assert_eq!(
            v["receivers"]["otlp/otel_test.otlp"]["protocols"]["http"]["endpoint"],
            Value::from("127.0.0.1:20001")
        );
        assert!(
            v["receivers"]["otlp/otel_test.otlp"]["protocols"]
                .get("grpc")
                .is_none()
        );

        let dd = &v["exporters"]["otlp_http/otel_test.datadog"];
        assert_eq!(dd["endpoint"], Value::from("http://127.0.0.1:9000/datadog"));
        assert_eq!(dd["retry_on_failure"]["enabled"], Value::from(false));
        assert_eq!(dd["sending_queue"]["enabled"], Value::from(false));
        assert!(
            dd.get("api").is_none(),
            "original exporter settings must not leak"
        );
        assert_eq!(keys(&v["exporters"]).len(), 3);
    }

    #[test]
    fn grpc_capture_target() {
        let config = CollectorConfig::parse(CONFIG, Path::new("c.yaml")).unwrap();
        let options = RewriteOptions {
            capture: CaptureTarget::Grpc {
                endpoint: "127.0.0.1:9001".into(),
                exporter_type: "otlp_grpc".into(),
            },
            health_check: false,
        };
        let out = rewrite(&config, &options, || Ok(1)).unwrap();
        let v: Value = serde_yaml::from_str(&out.yaml).unwrap();
        let e = &v["exporters"]["otlp_grpc/otel_test.otlp_http/backend"];
        assert_eq!(e["endpoint"], Value::from("127.0.0.1:9001"));
        assert_eq!(e["tls"]["insecure"], Value::from(true));
        assert_eq!(
            e["headers"][EXPORTER_HEADER],
            Value::from("otlp_http_backend")
        );
        assert!(
            e.get("encoding").is_none(),
            "encoding is an OTLP/HTTP-only setting"
        );
        assert_eq!(
            strings(&v["service"]["pipelines"]["traces/errors"]["exporters"]),
            ["otlp_grpc/otel_test.datadog"]
        );
    }

    #[test]
    fn processors_and_connectors_are_copied_verbatim() {
        let (_, v) = run(CONFIG);
        let original: Value = {
            let mut o: Value = serde_yaml::from_str(CONFIG).unwrap();
            o.apply_merge().unwrap();
            o
        };
        assert_eq!(v["processors"], original["processors"]);
        assert_eq!(v["connectors"], original["connectors"]);
        for id in ["traces", "traces/errors", "traces/default", "logs"] {
            assert_eq!(
                v["service"]["pipelines"][id].get("processors"),
                original["service"]["pipelines"][id].get("processors"),
                "processor list of {id} changed"
            );
        }
    }

    #[test]
    fn extensions_and_telemetry() {
        let (out, v) = run(CONFIG);
        assert_eq!(keys(&v["extensions"]), ["health_check/otel_test"]);
        assert_eq!(
            strings(&v["service"]["extensions"]),
            ["health_check/otel_test"]
        );
        assert_eq!(
            v["extensions"]["health_check/otel_test"]["endpoint"],
            Value::from("127.0.0.1:20003")
        );
        assert_eq!(out.health_check_port, Some(20003));

        assert_eq!(
            v["service"]["telemetry"]["metrics"]["level"],
            Value::from("none")
        );
        assert!(
            v["service"]["telemetry"]["metrics"]
                .get("address")
                .is_none()
        );
        assert_eq!(
            v["service"]["telemetry"]["logs"]["level"],
            Value::from("debug")
        );

        let notes = out.notes.join("\n");
        for expected in [
            "receiver otlp replaced by otlp/otel_test.otlp on 127.0.0.1:20001 (traces, logs)",
            "receiver prometheus removed: not used by any pipeline",
            "exporter datadog replaced by otlp_http/otel_test.datadog (traces)",
            "exporter kafka removed: not used by any pipeline",
            "extension basicauth/dd removed: not referenced by any processor or connector",
            "extension health_check removed",
            "extension health_check/otel_test added on 127.0.0.1:20003",
            "service.telemetry replaced: self-metrics disabled, logs at level debug to stderr",
        ] {
            assert!(
                notes.contains(expected),
                "missing note {expected:?} in:\n{notes}"
            );
        }
        assert!(
            out.yaml
                .starts_with("# Generated by otel-test from /src/collector.yaml.")
        );
    }

    #[test]
    fn keeps_extensions_referenced_by_processors_transitively() {
        let (out, v) = run(r#"
receivers: {otlp: }
processors:
  tail_sampling:
    storage: file_storage/ts
exporters: {debug: }
extensions:
  file_storage/ts:
    compaction: {directory: /tmp, on_start: true}
    encoder: my_encoding
  my_encoding: {}
  pprof: {}
service:
  extensions: [file_storage/ts, pprof]
  pipelines:
    traces: {receivers: [otlp], processors: [tail_sampling], exporters: [debug]}
"#);
        assert_eq!(
            keys(&v["extensions"]),
            ["file_storage/ts", "my_encoding", "health_check/otel_test"]
        );
        assert_eq!(
            strings(&v["service"]["extensions"]),
            ["file_storage/ts", "health_check/otel_test"]
        );
        assert!(
            out.notes
                .iter()
                .any(|n| n.starts_with("extension pprof removed"))
        );
        assert!(!out.notes.iter().any(|n| n.contains("service.telemetry")));
    }

    #[test]
    fn without_health_check_and_extensions() {
        let config =
            CollectorConfig::parse("receivers: {otlp: }\nexporters: {debug: }\nservice: {pipelines: {logs: {receivers: [otlp], exporters: [debug]}}}\n", Path::new("c.yaml"))
                .unwrap();
        let out = rewrite(
            &config,
            &RewriteOptions {
                health_check: false,
                ..options()
            },
            || Ok(1),
        )
        .unwrap();
        let v: Value = serde_yaml::from_str(&out.yaml).unwrap();
        assert!(v.get("extensions").is_none());
        assert!(v["service"].get("extensions").is_none());
        assert_eq!(out.health_check_port, None);
    }

    #[test]
    fn reserved_health_check_id_conflicts() {
        let config = CollectorConfig::parse(
            "receivers: {otlp: }\nexporters: {debug: }\nprocessors: {x: {e: health_check/otel_test}}\nextensions: {health_check/otel_test: }\nservice: {pipelines: {logs: {receivers: [otlp], processors: [x], exporters: [debug]}}}\n",
            Path::new("c.yaml"),
        )
        .unwrap();
        let err = rewrite(&config, &options(), || Ok(1)).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn port_allocation_failure_is_reported() {
        let config = CollectorConfig::parse(
            "receivers: {otlp: }\nexporters: {debug: }\nservice: {pipelines: {logs: {receivers: [otlp], exporters: [debug]}}}\n",
            Path::new("c.yaml"),
        )
        .unwrap();
        let err = rewrite(&config, &options(), || {
            Err(std::io::Error::other("no ports"))
        })
        .unwrap_err();
        assert!(matches!(err, RewriteError::Port(_)));
    }

    #[test]
    fn slugs_are_path_safe_and_unique() {
        let mut taken = HashSet::new();
        assert_eq!(unique_slug("otlp_http/eu", &mut taken), "otlp_http_eu");
        assert_eq!(unique_slug("otlp_http_eu", &mut taken), "otlp_http_eu-2");
        assert_eq!(unique_slug("otlp_http:eu", &mut taken), "otlp_http_eu-3");
        assert_eq!(unique_slug("datadog/ü x", &mut taken), "datadog___x");
    }

    #[test]
    fn injection_selection() {
        let (out, _) = run(CONFIG);
        assert_eq!(
            out.injection_for(Signal::Traces, None).unwrap().original,
            "otlp"
        );
        assert_eq!(
            out.injection_for(Signal::Logs, Some("filelog/app"))
                .unwrap()
                .port,
            20002
        );

        let e = out.injection_for(Signal::Logs, None).unwrap_err();
        assert!(
            e.contains("several receivers feed logs pipelines (otlp, filelog/app)"),
            "{e}"
        );
        let e = out
            .injection_for(Signal::Traces, Some("filelog/app"))
            .unwrap_err();
        assert!(e.contains("does not feed any traces pipeline"), "{e}");
        let e = out
            .injection_for(Signal::Traces, Some("prometheus"))
            .unwrap_err();
        assert!(e.contains("not used by any pipeline"), "{e}");
        let e = out.injection_for(Signal::Metrics, None).unwrap_err();
        assert!(e.contains("no metrics pipeline"), "{e}");
    }

    #[test]
    fn kubernetes_startup_hints() {
        let (r, _) = run(r#"
receivers: {otlp: }
processors:
  k8s_attributes: {auth_type: serviceAccount}
  k8sattributes/pass: {passthrough: true}
exporters: {debug: }
service:
  pipelines:
    traces: {receivers: [otlp], processors: [k8s_attributes, k8sattributes/pass], exporters: [debug]}
    logs: {receivers: [otlp], processors: [k8s_attributes], exporters: [debug]}
"#);
        assert_eq!(r.startup_hints.len(), 1, "{:?}", r.startup_hints);
        assert!(
            r.startup_hints[0].starts_with("processor k8s_attributes needs the Kubernetes API")
        );
        let (r, _) = run(
            "receivers: {otlp: }\nexporters: {debug: }\nservice: {pipelines: {traces: {receivers: [otlp], exporters: [debug]}}}\n",
        );
        assert!(r.startup_hints.is_empty());
    }

    #[test]
    fn env_references_survive_round_trip() {
        let (_, v) = run(r#"
receivers: {otlp: }
processors:
  resource:
    attributes: [{key: env, value: "${env:DEPLOY_ENV}", action: upsert}]
exporters: {debug: }
service:
  pipelines:
    traces: {receivers: [otlp], processors: [resource], exporters: [debug]}
"#);
        assert_eq!(
            v["processors"]["resource"]["attributes"][0]["value"],
            Value::from("${env:DEPLOY_ENV}")
        );
    }
}
