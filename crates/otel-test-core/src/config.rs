use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_yaml::{Mapping, Value};

use crate::spec::{Problem, Signal};

pub const SECTIONS: [&str; 5] = [
    "receivers",
    "processors",
    "exporters",
    "connectors",
    "extensions",
];

#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub source: PathBuf,
    pub(crate) root: Mapping,
    pub receivers: Vec<String>,
    pub processors: Vec<String>,
    pub exporters: Vec<String>,
    pub connectors: Vec<String>,
    pub extensions: Vec<String>,
    pub pipelines: Vec<Pipeline>,
    pub service_extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub id: String,
    pub signal: Signal,
    pub receivers: Vec<String>,
    pub processors: Vec<String>,
    pub exporters: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read Collector configuration {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid YAML in Collector configuration {}: {source}", path.display())]
    Syntax {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("invalid Collector configuration {}:{}", path.display(), format_problems(problems))]
    Invalid {
        path: PathBuf,
        problems: Vec<Problem>,
    },
}

fn format_problems(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(|p| format!("\n  - {}: {}", p.location, p.message))
        .collect()
}

impl CollectorConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, source: &Path) -> Result<Self, ConfigError> {
        let mut value: Value = serde_yaml::from_str(text).map_err(|e| ConfigError::Syntax {
            path: source.to_path_buf(),
            source: e,
        })?;
        value.apply_merge().map_err(|e| ConfigError::Syntax {
            path: source.to_path_buf(),
            source: e,
        })?;
        let mut p = Problems::default();
        let config = analyse(value, source, &mut p);
        match config {
            Some(c) if p.0.is_empty() => Ok(c),
            _ => Err(ConfigError::Invalid {
                path: source.to_path_buf(),
                problems: p.0,
            }),
        }
    }

    pub fn is_connector(&self, id: &str) -> bool {
        self.connectors.iter().any(|c| c == id)
    }

    pub fn reachable_pipelines(&self, receiver: &str, signal: Signal) -> Vec<&Pipeline> {
        let mut out: Vec<&Pipeline> = self
            .pipelines
            .iter()
            .filter(|p| p.signal == signal && p.receivers.iter().any(|r| r == receiver))
            .collect();
        let mut i = 0;
        while i < out.len() {
            let connectors: Vec<&String> = out[i]
                .exporters
                .iter()
                .filter(|e| self.is_connector(e))
                .collect();
            for c in connectors {
                for p in &self.pipelines {
                    if p.receivers.contains(c) && !out.iter().any(|o| o.id == p.id) {
                        out.push(p);
                    }
                }
            }
            i += 1;
        }
        out
    }

    pub fn component(&self, section: &str, id: &str) -> Option<&Value> {
        self.root.get(section)?.as_mapping()?.get(id)
    }
}

#[derive(Default)]
struct Problems(Vec<Problem>);

impl Problems {
    fn push(&mut self, location: impl Into<String>, message: impl fmt::Display) {
        self.0.push(Problem {
            location: location.into(),
            message: message.to_string(),
        });
    }
}

static COMPONENT_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z][0-9a-zA-Z_]{0,62}$").unwrap());
static COMPONENT_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^\pZ\pC\pS]+$").unwrap());

pub(crate) fn split_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once('/') {
        Some((t, n)) => (t, Some(n)),
        None => (id, None),
    }
}

fn check_id(id: &str) -> Result<(), String> {
    if id.contains("${") {
        return Err(
            "component IDs containing `${...}` provider references can not be analysed".into(),
        );
    }
    let (ty, name) = split_id(id);
    if !COMPONENT_TYPE.is_match(ty) {
        return Err(format!("invalid component type {ty:?}"));
    }
    if let Some(name) = name
        && !COMPONENT_NAME.is_match(name)
    {
        return Err(format!("invalid component name {name:?}"));
    }
    Ok(())
}

fn structural<'a>(
    value: &'a Value,
    location: &str,
    expected: &str,
    p: &mut Problems,
) -> Option<&'a Value> {
    match value {
        Value::String(s) if s.contains("${") => {
            p.push(
                location,
                format!("provider reference {s:?} in place of a {expected} is not supported; inline the content"),
            );
            None
        }
        _ => Some(value),
    }
}

fn analyse(value: Value, source: &Path, p: &mut Problems) -> Option<CollectorConfig> {
    let Value::Mapping(root) = value else {
        p.push(
            "<root>",
            "expected a mapping with `receivers`, `exporters` and `service`",
        );
        return None;
    };

    let mut ids: [Vec<String>; 5] = Default::default();
    for (i, section) in SECTIONS.iter().enumerate() {
        let Some(v) = root.get(*section) else {
            continue;
        };
        let Some(v) = structural(v, section, "mapping", p) else {
            continue;
        };
        match v {
            Value::Null => {}
            Value::Mapping(m) => {
                for key in m.keys() {
                    match key.as_str() {
                        Some(id) => match check_id(id) {
                            Ok(()) => ids[i].push(id.to_owned()),
                            Err(e) => p.push(format!("{section}.{id}"), e),
                        },
                        None => p.push(
                            section.to_string(),
                            format!("component IDs must be strings, found {key:?}"),
                        ),
                    }
                }
            }
            _ => p.push(*section, "expected a mapping of component IDs"),
        }
    }
    let [receivers, processors, exporters, connectors, extensions] = ids;

    for c in &connectors {
        if receivers.contains(c) || exporters.contains(c) {
            p.push(
                format!("connectors.{c}"),
                "ID is also defined as a receiver or exporter; pipeline references would be ambiguous",
            );
        }
    }

    let empty = Mapping::new();
    let service = match root.get("service") {
        None => {
            p.push(
                "service",
                "missing; a Collector config needs `service.pipelines`",
            );
            &empty
        }
        Some(v) => match structural(v, "service", "mapping", p) {
            Some(Value::Mapping(m)) => m,
            Some(_) => {
                p.push("service", "expected a mapping");
                &empty
            }
            None => &empty,
        },
    };

    let service_extensions = match service.get("extensions") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => string_list(v, "service.extensions", p),
    };
    for e in &service_extensions {
        if !extensions.contains(e) {
            p.push(
                "service.extensions",
                format!("extension {e:?} is not defined in `extensions`"),
            );
        }
    }

    let mut pipelines = Vec::new();
    match service.get("pipelines") {
        None | Some(Value::Null) if service.is_empty() => {}
        None | Some(Value::Null) => p.push(
            "service.pipelines",
            "missing; at least one pipeline is required",
        ),
        Some(v) => match structural(v, "service.pipelines", "mapping", p) {
            Some(Value::Mapping(m)) => {
                if m.is_empty() {
                    p.push("service.pipelines", "at least one pipeline is required");
                }
                for (k, v) in m {
                    let Some(id) = k.as_str() else {
                        p.push(
                            "service.pipelines",
                            format!("pipeline IDs must be strings, found {k:?}"),
                        );
                        continue;
                    };
                    if let Some(pl) = pipeline(id, v, p) {
                        pipelines.push(pl);
                    }
                }
            }
            Some(_) => p.push("service.pipelines", "expected a mapping of pipeline IDs"),
            None => {}
        },
    }

    let defined = |list: &[String], id: &str| list.iter().any(|x| x == id);
    for pl in &pipelines {
        let loc = format!("service.pipelines.{}", pl.id);
        for r in &pl.receivers {
            if !defined(&receivers, r) && !defined(&connectors, r) {
                p.push(
                    format!("{loc}.receivers"),
                    format!("{r:?} is not defined in `receivers` or `connectors`"),
                );
            }
        }
        for x in &pl.processors {
            if !defined(&processors, x) {
                p.push(
                    format!("{loc}.processors"),
                    format!("{x:?} is not defined in `processors`"),
                );
            }
        }
        for e in &pl.exporters {
            if !defined(&exporters, e) && !defined(&connectors, e) {
                p.push(
                    format!("{loc}.exporters"),
                    format!("{e:?} is not defined in `exporters` or `connectors`"),
                );
            }
        }
    }

    Some(CollectorConfig {
        source: source.to_path_buf(),
        root,
        receivers,
        processors,
        exporters,
        connectors,
        extensions,
        pipelines,
        service_extensions,
    })
}

fn pipeline(id: &str, value: &Value, p: &mut Problems) -> Option<Pipeline> {
    let loc = format!("service.pipelines.{id}");
    let (ty, _) = split_id(id);
    let signal = match ty {
        "traces" => Signal::Traces,
        "metrics" => Signal::Metrics,
        "logs" => Signal::Logs,
        "profiles" => {
            p.push(loc, "profiles pipelines are not supported by otel-test");
            return None;
        }
        other => {
            p.push(
                loc,
                format!("unknown pipeline type {other:?} (expected traces, metrics or logs)"),
            );
            return None;
        }
    };
    let m = match structural(value, &loc, "mapping", p)? {
        Value::Mapping(m) => m,
        _ => {
            p.push(loc, "expected a mapping with `receivers` and `exporters`");
            return None;
        }
    };
    let list = |key: &str, required: bool, p: &mut Problems| match m.get(key) {
        None | Some(Value::Null) => {
            if required {
                p.push(format!("{loc}.{key}"), "at least one entry is required");
            }
            Vec::new()
        }
        Some(v) => {
            let l = string_list(v, &format!("{loc}.{key}"), p);
            if required && l.is_empty() {
                p.push(format!("{loc}.{key}"), "at least one entry is required");
            }
            l
        }
    };
    Some(Pipeline {
        id: id.to_owned(),
        signal,
        receivers: list("receivers", true, p),
        processors: list("processors", false, p),
        exporters: list("exporters", true, p),
    })
}

fn string_list(value: &Value, location: &str, p: &mut Problems) -> Vec<String> {
    let Some(value) = structural(value, location, "list", p) else {
        return Vec::new();
    };
    let Value::Sequence(seq) = value else {
        p.push(location, "expected a list of component IDs");
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        match item.as_str() {
            Some(s) if s.contains("${") => p.push(
                location,
                format!("provider reference {s:?} in a component list is not supported"),
            ),
            Some(s) => {
                if !seen.insert(s) {
                    p.push(location, format!("{s:?} is listed more than once"));
                }
                out.push(s.to_owned());
            }
            None => p.push(
                location,
                format!("expected a component ID string, found {item:?}"),
            ),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(yaml: &str) -> CollectorConfig {
        CollectorConfig::parse(yaml, Path::new("collector.yaml")).unwrap_or_else(|e| panic!("{e}"))
    }

    fn problems(yaml: &str) -> Vec<(String, String)> {
        match CollectorConfig::parse(yaml, Path::new("collector.yaml")) {
            Err(ConfigError::Invalid { problems, .. }) => problems
                .into_iter()
                .map(|p| (p.location, p.message))
                .collect(),
            Err(e) => panic!("expected problems, got {e}"),
            Ok(_) => panic!("expected config to be rejected"),
        }
    }

    const FULL: &str = r#"
receivers:
  otlp:
    protocols: {grpc: {}}
  filelog/app:
    include: [/var/log/app.log]
processors:
  batch: {}
  attributes/redact:
    actions: [{key: password, action: delete}]
exporters:
  otlp_http/backend:
    endpoint: https://example.com
  debug:
connectors:
  forward/fanout:
extensions:
  health_check:
service:
  extensions: [health_check]
  pipelines:
    traces:
      receivers: [otlp]
      processors: [attributes/redact, batch]
      exporters: [forward/fanout]
    traces/out:
      receivers: [forward/fanout]
      exporters: [otlp_http/backend, debug]
    logs:
      receivers: [filelog/app, otlp]
      exporters: [debug]
"#;

    #[test]
    fn models_components_and_pipelines() {
        let c = parse_ok(FULL);
        assert_eq!(c.receivers, ["otlp", "filelog/app"]);
        assert_eq!(c.processors, ["batch", "attributes/redact"]);
        assert_eq!(c.exporters, ["otlp_http/backend", "debug"]);
        assert_eq!(c.connectors, ["forward/fanout"]);
        assert_eq!(c.extensions, ["health_check"]);
        assert_eq!(c.service_extensions, ["health_check"]);
        assert!(c.is_connector("forward/fanout"));
        assert!(!c.is_connector("otlp"));
        assert_eq!(
            c.pipelines,
            vec![
                Pipeline {
                    id: "traces".into(),
                    signal: Signal::Traces,
                    receivers: vec!["otlp".into()],
                    processors: vec!["attributes/redact".into(), "batch".into()],
                    exporters: vec!["forward/fanout".into()],
                },
                Pipeline {
                    id: "traces/out".into(),
                    signal: Signal::Traces,
                    receivers: vec!["forward/fanout".into()],
                    processors: vec![],
                    exporters: vec!["otlp_http/backend".into(), "debug".into()],
                },
                Pipeline {
                    id: "logs".into(),
                    signal: Signal::Logs,
                    receivers: vec!["filelog/app".into(), "otlp".into()],
                    processors: vec![],
                    exporters: vec!["debug".into()],
                },
            ]
        );
    }

    #[test]
    fn reachability_follows_connectors() {
        let c = parse_ok(FULL);
        let ids = |r, s| {
            c.reachable_pipelines(r, s)
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("otlp", Signal::Traces), ["traces", "traces/out"]);
        assert_eq!(ids("otlp", Signal::Logs), ["logs"]);
        assert_eq!(ids("filelog/app", Signal::Logs), ["logs"]);
        assert!(ids("filelog/app", Signal::Traces).is_empty());
        assert!(ids("otlp", Signal::Metrics).is_empty());
    }

    #[test]
    fn applies_merge_keys() {
        let c = parse_ok(
            r#"
receivers: {otlp: }
processors:
  attributes/a: &shared
    actions: [{key: password, action: delete}]
  attributes/b:
    <<: *shared
    include: {match_type: strict, services: [x]}
exporters: {debug: }
service:
  pipelines:
    traces: {receivers: [otlp], processors: [attributes/b], exporters: [debug]}
"#,
        );
        let b = c.component("processors", "attributes/b").unwrap();
        assert!(b.get("<<").is_none());
        assert_eq!(b["actions"][0]["key"], Value::from("password"));
        assert_eq!(b["include"]["match_type"], Value::from("strict"));
    }

    #[test]
    fn reports_undefined_references() {
        let p = problems(
            r#"
receivers: {otlp: }
exporters: {debug: }
extensions: {pprof: }
service:
  extensions: [pprof, zpages]
  pipelines:
    traces:
      receivers: [otlp, jaeger]
      processors: [batch]
      exporters: [debug, routing]
"#,
        );
        assert_eq!(
            p,
            [
                (
                    "service.extensions".into(),
                    "extension \"zpages\" is not defined in `extensions`".into()
                ),
                (
                    "service.pipelines.traces.receivers".into(),
                    "\"jaeger\" is not defined in `receivers` or `connectors`".into()
                ),
                (
                    "service.pipelines.traces.processors".into(),
                    "\"batch\" is not defined in `processors`".into()
                ),
                (
                    "service.pipelines.traces.exporters".into(),
                    "\"routing\" is not defined in `exporters` or `connectors`".into()
                ),
            ]
        );
    }

    #[test]
    fn rejects_unanalysable_structure() {
        let p = problems(
            r#"
receivers: {otlp: }
exporters: ${file:exporters.yaml}
service:
  pipelines:
    traces:
      receivers: ${env:RECEIVERS}
      exporters: [debug, "${env:EXTRA}"]
    profiles:
      receivers: [otlp]
      exporters: [debug]
    spans:
      receivers: [otlp]
"#,
        );
        let locations: Vec<&str> = p.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            locations,
            [
                "exporters",
                "service.pipelines.traces.receivers",
                "service.pipelines.traces.receivers",
                "service.pipelines.traces.exporters",
                "service.pipelines.profiles",
                "service.pipelines.spans",
                "service.pipelines.traces.exporters",
            ],
            "{p:#?}"
        );
        assert!(p[0].1.contains("provider reference"));
        assert!(p[4].1.contains("profiles pipelines are not supported"));
        assert!(p[5].1.contains("unknown pipeline type \"spans\""));
    }

    #[test]
    fn validates_pipeline_shape_and_ids() {
        let p = problems(
            r#"
receivers:
  "bad type": {}
  otlp/: {}
  otlp: {}
exporters: {debug: }
connectors: {debug: }
service:
  pipelines:
    traces:
      receivers: [otlp, otlp]
      exporters: []
    logs: [otlp]
"#,
        );
        let got: Vec<String> = p.iter().map(|(l, m)| format!("{l}: {m}")).collect();
        assert_eq!(
            got,
            [
                "receivers.bad type: invalid component type \"bad type\"",
                "receivers.otlp/: invalid component name \"\"",
                "connectors.debug: ID is also defined as a receiver or exporter; pipeline references would be ambiguous",
                "service.pipelines.traces.receivers: \"otlp\" is listed more than once",
                "service.pipelines.traces.exporters: at least one entry is required",
                "service.pipelines.logs: expected a mapping with `receivers` and `exporters`",
            ],
            "{got:#?}"
        );
    }

    #[test]
    fn requires_pipelines() {
        assert_eq!(problems("receivers: {otlp: }\n")[0].0, "service");
        assert_eq!(
            problems("service:\n  extensions: []\n")[0].0,
            "service.pipelines"
        );
        assert_eq!(
            problems("service: {pipelines: {}}\n")[0].1,
            "at least one pipeline is required"
        );
        assert_eq!(problems("- a\n")[0].0, "<root>");
    }

    #[test]
    fn syntax_error_has_position() {
        let e = CollectorConfig::parse("receivers:\n  otlp: [\n", Path::new("c.yaml")).unwrap_err();
        assert!(matches!(e, ConfigError::Syntax { .. }));
        assert!(e.to_string().contains("line"), "{e}");
    }
}
