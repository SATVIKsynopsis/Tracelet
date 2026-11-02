use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use opentelemetry_proto::tonic::common::v1::any_value::Value as Any;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use otel_test_capture::{Captured, Payload};

use crate::fixture::signal_items;
use crate::spec::{
    Assertion, AttrValue, AttributeAssertion, AttributeCheck, AttributeLevel, Signal,
};

const MAX_LISTED: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertionResult {
    pub assertion: String,
    pub passed: bool,
    pub details: Vec<String>,
}

struct Item<'a> {
    exporter: &'a str,
    resource: &'a [KeyValue],
    scope: &'a [KeyValue],
    attributes: &'a [KeyValue],
    label: String,
}

fn items(captured: &[Captured]) -> Vec<Item<'_>> {
    let mut out = Vec::new();
    for c in captured {
        let exporter = c.exporter.as_str();
        match &c.payload {
            Payload::Traces(r) => {
                for rs in &r.resource_spans {
                    let resource = rs.resource.as_ref().map_or(&[][..], |r| &r.attributes);
                    for ss in &rs.scope_spans {
                        let scope = ss.scope.as_ref().map_or(&[][..], |s| &s.attributes);
                        for span in &ss.spans {
                            out.push(Item {
                                exporter,
                                resource,
                                scope,
                                attributes: &span.attributes,
                                label: format!("span {:?}", span.name),
                            });
                        }
                    }
                }
            }
            Payload::Logs(r) => {
                for rl in &r.resource_logs {
                    let resource = rl.resource.as_ref().map_or(&[][..], |r| &r.attributes);
                    for sl in &rl.scope_logs {
                        let scope = sl.scope.as_ref().map_or(&[][..], |s| &s.attributes);
                        for log in &sl.log_records {
                            let body = log
                                .body
                                .as_ref()
                                .map(show)
                                .unwrap_or_else(|| "<no body>".into());
                            out.push(Item {
                                exporter,
                                resource,
                                scope,
                                attributes: &log.attributes,
                                label: format!("log record {}", truncate(&body, 60)),
                            });
                        }
                    }
                }
            }
            Payload::Metrics(r) => {
                for rm in &r.resource_metrics {
                    let resource = rm.resource.as_ref().map_or(&[][..], |r| &r.attributes);
                    for sm in &rm.scope_metrics {
                        let scope = sm.scope.as_ref().map_or(&[][..], |s| &s.attributes);
                        for m in &sm.metrics {
                            let points: Vec<&[KeyValue]> = match &m.data {
                                Some(Data::Gauge(g)) => {
                                    g.data_points.iter().map(|p| &p.attributes[..]).collect()
                                }
                                Some(Data::Sum(s)) => {
                                    s.data_points.iter().map(|p| &p.attributes[..]).collect()
                                }
                                Some(Data::Histogram(h)) => {
                                    h.data_points.iter().map(|p| &p.attributes[..]).collect()
                                }
                                Some(Data::ExponentialHistogram(h)) => {
                                    h.data_points.iter().map(|p| &p.attributes[..]).collect()
                                }
                                Some(Data::Summary(s)) => {
                                    s.data_points.iter().map(|p| &p.attributes[..]).collect()
                                }
                                None => Vec::new(),
                            };
                            for (i, attributes) in points.into_iter().enumerate() {
                                out.push(Item {
                                    exporter,
                                    resource,
                                    scope,
                                    attributes,
                                    label: format!("data point #{i} of {:?}", m.name),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

pub type ExporterSignals = BTreeMap<String, BTreeSet<Signal>>;

fn payload_signal(p: &Payload) -> Signal {
    match p {
        Payload::Traces(_) => Signal::Traces,
        Payload::Metrics(_) => Signal::Metrics,
        Payload::Logs(_) => Signal::Logs,
    }
}

type Counts<'a> = BTreeMap<&'a str, BTreeMap<Signal, usize>>;

fn counts(captured: &[Captured]) -> Counts<'_> {
    let mut m = Counts::new();
    for c in captured.iter().filter(|c| c.payload.item_count() > 0) {
        *m.entry(c.exporter.as_str())
            .or_default()
            .entry(payload_signal(&c.payload))
            .or_default() += c.payload.item_count();
    }
    m
}

fn noun(signals: &BTreeSet<Signal>, n: usize) -> String {
    match signals.iter().collect::<Vec<_>>().as_slice() {
        [s] => signal_items(**s, n),
        _ => if n == 1 { "item" } else { "items" }.to_owned(),
    }
}

fn describe_counts(counts: &Counts) -> String {
    if counts.is_empty() {
        return "nothing".into();
    }
    counts
        .iter()
        .map(|(e, by_signal)| {
            let parts: Vec<String> = by_signal
                .iter()
                .map(|(s, n)| format!("{n} {}", signal_items(*s, *n)))
                .collect();
            format!("{e}: {}", parts.join(" + "))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

struct Nouns<'a> {
    fixture: Signal,
    exporters: &'a ExporterSignals,
    counts: &'a Counts<'a>,
}

impl Nouns<'_> {
    fn signals(&self, exporter: Option<&str>) -> BTreeSet<Signal> {
        let seen: BTreeSet<Signal> = self
            .counts
            .iter()
            .filter(|(e, _)| exporter.is_none_or(|x| x == **e))
            .flat_map(|(_, by_signal)| by_signal.keys().copied())
            .collect();
        if !seen.is_empty() {
            return seen;
        }
        exporter
            .and_then(|e| self.exporters.get(e))
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| BTreeSet::from([self.fixture]))
    }

    fn noun(&self, exporter: Option<&str>, n: usize) -> String {
        noun(&self.signals(exporter), n)
    }

    fn quantity(&self, exporter: Option<&str>, n: usize) -> String {
        format!("{n} {}", self.noun(exporter, n))
    }
}

fn exporter_total(counts: &Counts, exporter: &str) -> usize {
    counts.get(exporter).map_or(0, |m| m.values().sum())
}

pub fn evaluate(
    assertion: &Assertion,
    signal: Signal,
    exporters: &ExporterSignals,
    captured: &[Captured],
) -> AssertionResult {
    let all = counts(captured);
    let nouns = Nouns {
        fixture: signal,
        exporters,
        counts: &all,
    };
    let total: usize = all.values().flat_map(|m| m.values()).sum();
    let (passed, details) = match assertion {
        Assertion::Dropped => {
            if total == 0 {
                (true, vec![])
            } else {
                (
                    false,
                    vec![
                        "Expected: nothing exported".to_owned(),
                        format!("Actual:   {} exported", nouns.quantity(None, total)),
                        format!("Received: {}", describe_counts(&all)),
                    ],
                )
            }
        }
        Assertion::ExporterReached { exporter } => {
            let n = exporter_total(&all, exporter);
            if n > 0 {
                (true, vec![])
            } else {
                (
                    false,
                    vec![
                        format!("Expected: at least {}", nouns.quantity(Some(exporter), 1)),
                        format!("Actual:   {}", nouns.quantity(Some(exporter), 0)),
                        format!("Exporter: {exporter}"),
                        format!("Received: {}", describe_counts(&all)),
                    ],
                )
            }
        }
        Assertion::ExporterCount { exporter, count } => {
            let n = exporter_total(&all, exporter);
            if n as u64 == *count {
                (true, vec![])
            } else {
                (
                    false,
                    vec![
                        format!(
                            "Expected: {}",
                            nouns.quantity(Some(exporter), *count as usize)
                        ),
                        format!("Actual:   {}", nouns.quantity(Some(exporter), n)),
                        format!("Exporter: {exporter}"),
                        format!("Received: {}", describe_counts(&all)),
                    ],
                )
            }
        }
        Assertion::Attribute(a) => attribute(a, &nouns, captured, &all),
    };
    AssertionResult {
        assertion: assertion.to_string(),
        passed,
        details,
    }
}

fn attribute(
    a: &AttributeAssertion,
    nouns: &Nouns,
    captured: &[Captured],
    all: &Counts,
) -> (bool, Vec<String>) {
    let all_items = items(captured);
    let scoped: Vec<&Item> = all_items
        .iter()
        .filter(|i| a.exporter.as_deref().is_none_or(|e| e == i.exporter))
        .collect();
    let at = a.exporter.as_deref();
    let noun = nouns.noun(at, 2);
    let one = nouns.noun(at, 1);
    let where_ = match &a.exporter {
        Some(e) => format!(" at {e}"),
        None => String::new(),
    };
    let level = match a.level {
        AttributeLevel::Any => "",
        AttributeLevel::Resource => " (resource attribute)",
        AttributeLevel::Scope => " (scope attribute)",
        AttributeLevel::Item => " (item attribute)",
    };
    let expected = match &a.check {
        AttributeCheck::Present => format!("{}{level} present on every {one}{where_}", a.key,),
        AttributeCheck::Absent => format!("{}{level} absent from every {one}{where_}", a.key,),
        AttributeCheck::Equals(v) => {
            format!(
                "{}{level} = {} ({}) on every {one}{where_}",
                a.key,
                v,
                type_of_expected(v),
            )
        }
        AttributeCheck::Matches { pattern, .. } => {
            format!(
                "{}{level} matching /{pattern}/ on every {one}{where_}",
                a.key,
            )
        }
    };

    if scoped.is_empty() {
        let mut details = vec![
            format!("Expected: {expected}"),
            format!("Actual:   no {noun} captured{where_}"),
        ];
        if a.exporter.is_some() && !all.is_empty() {
            details.push(format!("Received: {}", describe_counts(all)));
        }
        return (false, details);
    }

    let mut failures = Vec::new();
    for item in &scoped {
        if let Some(problem) = check(a, item) {
            failures.push(format!("{} at {}: {problem}", item.label, item.exporter));
        }
    }
    if failures.is_empty() {
        return (true, vec![]);
    }
    let mut actual = format!("{} of {} {noun} failed", failures.len(), scoped.len());
    for f in failures.iter().take(MAX_LISTED) {
        let _ = write!(actual, "\n            {f}");
    }
    if failures.len() > MAX_LISTED {
        let _ = write!(
            actual,
            "\n            … and {} more",
            failures.len() - MAX_LISTED
        );
    }
    (
        false,
        vec![
            format!("Expected: {expected}"),
            format!("Actual:   {actual}"),
        ],
    )
}

fn check(a: &AttributeAssertion, item: &Item) -> Option<String> {
    let levels: &[(&str, &[KeyValue])] = &[
        ("item", item.attributes),
        ("scope", item.scope),
        ("resource", item.resource),
    ];
    let wanted: Vec<&(&str, &[KeyValue])> = levels
        .iter()
        .filter(|(name, _)| match a.level {
            AttributeLevel::Any => true,
            AttributeLevel::Item => *name == "item",
            AttributeLevel::Scope => *name == "scope",
            AttributeLevel::Resource => *name == "resource",
        })
        .collect();

    if let AttributeCheck::Absent = a.check {
        let found: Vec<String> = wanted
            .iter()
            .filter_map(|(name, attrs)| {
                lookup(attrs, &a.key).map(|v| format!("{} = {} ({name})", a.key, show_opt(v)))
            })
            .collect();
        return (!found.is_empty()).then(|| found.join(", "));
    }

    let Some((level, value)) = wanted
        .iter()
        .find_map(|(name, attrs)| lookup(attrs, &a.key).map(|v| (*name, v)))
    else {
        return Some("attribute missing".into());
    };
    let shown = format!("{} = {}", a.key, show_opt(value));
    let value = value.and_then(|v| v.value.as_ref());
    match &a.check {
        AttributeCheck::Present => None,
        AttributeCheck::Absent => unreachable!("handled above"),
        AttributeCheck::Equals(want) => {
            if value.is_some_and(|v| equals(v, want)) {
                None
            } else if value.is_some_and(|v| type_of(v) != type_of_expected(want)) {
                Some(format!(
                    "{shown} ({}, {level})",
                    value.map_or("empty", type_of)
                ))
            } else {
                Some(format!("{shown} ({level})"))
            }
        }
        AttributeCheck::Matches { regex, .. } => match value {
            Some(Any::StringValue(s)) if regex.is_match(s) => None,
            Some(Any::StringValue(_)) => Some(format!("{shown} ({level})")),
            other => Some(format!(
                "{shown} is a {}, not a string ({level})",
                other.map_or("empty value", type_of)
            )),
        },
    }
}

fn lookup<'a>(attrs: &'a [KeyValue], key: &str) -> Option<Option<&'a AnyValue>> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .map(|kv| kv.value.as_ref())
}

fn equals(v: &Any, want: &AttrValue) -> bool {
    match (v, want) {
        (Any::StringValue(a), AttrValue::String(b)) => a == b,
        (Any::IntValue(a), AttrValue::Int(b)) => a == b,
        (Any::DoubleValue(a), AttrValue::Double(b)) => a == b,
        (Any::BoolValue(a), AttrValue::Bool(b)) => a == b,
        _ => false,
    }
}

fn type_of(v: &Any) -> &'static str {
    match v {
        Any::StringValue(_) => "string",
        Any::IntValue(_) => "int",
        Any::DoubleValue(_) => "double",
        Any::BoolValue(_) => "bool",
        Any::BytesValue(_) => "bytes",
        Any::ArrayValue(_) => "array",
        Any::KvlistValue(_) => "map",
        Any::StringValueStrindex(_) => "string",
    }
}

fn type_of_expected(v: &AttrValue) -> &'static str {
    match v {
        AttrValue::String(_) => "string",
        AttrValue::Int(_) => "int",
        AttrValue::Double(_) => "double",
        AttrValue::Bool(_) => "bool",
    }
}

fn show_opt(v: Option<&AnyValue>) -> String {
    v.map(show).unwrap_or_else(|| "<empty>".into())
}

fn show(v: &AnyValue) -> String {
    match &v.value {
        None => "<empty>".into(),
        Some(Any::StringValue(s)) => format!("{s:?}"),
        Some(Any::IntValue(i)) => i.to_string(),
        Some(Any::DoubleValue(d)) => d.to_string(),
        Some(Any::BoolValue(b)) => b.to_string(),
        Some(Any::BytesValue(b)) => format!("<{} bytes>", b.len()),
        Some(Any::ArrayValue(a)) => format!(
            "[{}]",
            a.values.iter().map(show).collect::<Vec<_>>().join(", ")
        ),
        Some(Any::KvlistValue(m)) => format!(
            "{{{}}}",
            m.values
                .iter()
                .map(|kv| format!("{}: {}", kv.key, show_opt(kv.value.as_ref())))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Some(Any::StringValueStrindex(i)) => format!("<string table #{i}>"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Instant;

    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use otel_test_capture::Protocol;

    fn fixture<T: serde::de::DeserializeOwned>(path: &str) -> T {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .join(path);
        serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
    }

    fn captured(exporter: &str, payload: Payload) -> Captured {
        Captured {
            exporter: exporter.into(),
            protocol: Protocol::HttpProtobuf,
            received_at: Instant::now(),
            payload,
        }
    }

    fn login_span() -> Payload {
        Payload::Traces(fixture::<ExportTraceServiceRequest>(
            "traces/login-span.json",
        ))
    }

    fn error_span() -> Payload {
        Payload::Traces(fixture::<ExportTraceServiceRequest>(
            "traces/error-span.json",
        ))
    }

    fn assertion(yaml: &str) -> Assertion {
        let spec = format!(
            "collector: c\ntests:\n  - name: t\n    input: f\n    expect:\n      - {yaml}\n"
        );
        crate::spec::parse(&spec, Path::new("s.yaml"))
            .unwrap()
            .tests[0]
            .expect[0]
            .clone()
    }

    fn eval(yaml: &str, signal: Signal, captured: &[Captured]) -> AssertionResult {
        evaluate(&assertion(yaml), signal, &ExporterSignals::new(), captured)
    }

    fn passes(yaml: &str, captured: &[Captured]) {
        let r = eval(yaml, Signal::Traces, captured);
        assert!(r.passed, "{yaml} should pass: {:#?}", r.details);
        assert!(r.details.is_empty());
    }

    fn fails(yaml: &str, captured: &[Captured]) -> String {
        let r = eval(yaml, Signal::Traces, captured);
        assert!(!r.passed, "{yaml} should fail");
        r.details.join("\n")
    }

    #[test]
    fn attribute_checks_on_spans() {
        let c = [captured("otlp_http/backend", login_span())];
        passes("{attribute: password, present: true}", &c);
        passes("{attribute: http.route, equals: /login}", &c);
        passes("{attribute: http.response.status_code, equals: 200}", &c);
        passes("{attribute: user.name, matches: \"ali.*\"}", &c);
        passes("{attribute: missing.key, absent: true}", &c);
        passes("{attribute: service.name, equals: auth}", &c);
        passes(
            "{attribute: service.name, level: resource, matches: \"a.*\"}",
            &c,
        );
        passes("{attribute: user.name, level: item, present: true}", &c);

        let d = fails("{attribute: password, absent: true}", &c);
        assert_eq!(
            d,
            "Expected: password absent from every span\n\
             Actual:   1 of 1 spans failed\n            \
             span \"POST /login\" at otlp_http/backend: password = \"hunter2\" (item)"
        );
        let d = fails("{attribute: service.name, level: item, present: true}", &c);
        assert!(
            d.contains("service.name (item attribute) present on every span"),
            "{d}"
        );
        assert!(d.contains("attribute missing"), "{d}");
        assert!(
            fails("{attribute: service.name, absent: true}", &c)
                .contains("service.name = \"auth\" (resource)")
        );
        let d = fails("{attribute: user.name, matches: \"ali\"}", &c);
        assert!(d.contains("matching /ali/"), "anchored regex: {d}");
    }

    #[test]
    fn equality_is_type_strict() {
        let c = [captured("datadog", error_span())];
        passes("{attribute: http.response.status_code, equals: 500}", &c);
        let d = fails(
            "{attribute: http.response.status_code, equals: \"500\"}",
            &c,
        );
        assert!(
            d.contains("Expected: http.response.status_code = \"500\" (string)"),
            "{d}"
        );
        assert!(
            d.contains("http.response.status_code = 500 (int, item)"),
            "{d}"
        );
        let d = fails("{attribute: http.response.status_code, equals: 404}", &c);
        assert!(d.contains("= 500 (item)"), "{d}");
        let d = fails(
            "{attribute: http.response.status_code, matches: \"5..\"}",
            &c,
        );
        assert!(d.contains("is a int, not a string"), "{d}");
        passes("{attribute: error.type, equals: \"503\"}", &c);
        assert!(fails("{attribute: error.type, equals: 503}", &c).contains("(string, item)"));
    }

    #[test]
    fn attribute_assertions_fail_without_data() {
        let d = fails("{attribute: password, absent: true}", &[]);
        assert_eq!(
            d,
            "Expected: password absent from every span\nActual:   no spans captured"
        );
        let c = [captured("otlp_http/backend", login_span())];
        let d = fails("{attribute: password, absent: true, exporter: datadog}", &c);
        assert!(d.contains("Actual:   no spans captured at datadog"), "{d}");
        assert!(d.contains("Received: otlp_http/backend: 1 span"), "{d}");
    }

    #[test]
    fn exporter_scope_filters_items() {
        let mut redacted = login_span();
        if let Payload::Traces(r) = &mut redacted {
            r.resource_spans[0].scope_spans[0].spans[0]
                .attributes
                .retain(|kv| kv.key != "password");
        }
        let c = [
            captured("debug", login_span()),
            captured("otlp_http/backend", redacted),
        ];
        passes(
            "{attribute: password, absent: true, exporter: otlp_http/backend}",
            &c,
        );
        let d = fails("{attribute: password, absent: true}", &c);
        assert!(d.contains("1 of 2 spans failed"), "{d}");
        assert!(d.contains("at debug"), "{d}");
    }

    #[test]
    fn exporter_and_dropped_assertions() {
        let c = [
            captured("datadog", error_span()),
            captured("otlp_http/backend", login_span()),
            captured("otlp_http/backend", login_span()),
        ];
        passes("{exporter: datadog}", &c);
        passes("{exporter: datadog, count: 1}", &c);
        passes("{exporter: otlp_http/backend, count: 2}", &c);
        passes("{exporter: debug, count: 0}", &c);
        assert!(eval("{dropped: true}", Signal::Traces, &[]).passed);

        let d = fails("{exporter: datadog, count: 3}", &c);
        assert_eq!(
            d,
            "Expected: 3 spans\nActual:   1 span\nExporter: datadog\nReceived: datadog: 1 span, otlp_http/backend: 2 spans"
        );
        let d = fails("{exporter: debug}", &c);
        assert!(
            d.starts_with("Expected: at least 1 span\nActual:   0 spans\nExporter: debug"),
            "{d}"
        );
        let d = fails("{dropped: true}", &c);
        assert!(d.contains("Actual:   3 spans exported"), "{d}");
        let d = fails("{exporter: datadog}", &[]);
        assert!(d.contains("Received: nothing"), "{d}");
    }

    #[test]
    fn logs_and_metrics() {
        let logs = [captured(
            "debug",
            Payload::Logs(fixture::<ExportLogsServiceRequest>("logs/login-log.json")),
        )];
        let r = eval("{attribute: password, absent: true}", Signal::Logs, &logs);
        assert!(!r.passed);
        assert!(
            r.details[0].contains("absent from every log record"),
            "{:?}",
            r.details
        );
        assert!(
            r.details[1].contains("log record \"login attempt\" at debug"),
            "{:?}",
            r.details
        );
        assert_eq!(
            eval("{exporter: debug, count: 1}", Signal::Logs, &logs).details,
            Vec::<String>::new()
        );

        let metrics = [captured(
            "prom",
            Payload::Metrics(fixture::<ExportMetricsServiceRequest>(
                "metrics/http-requests.json",
            )),
        )];
        assert!(eval("{exporter: prom, count: 2}", Signal::Metrics, &metrics).passed);
        assert!(
            eval(
                "{attribute: http.route, equals: /api/orders}",
                Signal::Metrics,
                &metrics
            )
            .passed
        );
        let r = eval(
            "{attribute: http.response.status_code, equals: 200}",
            Signal::Metrics,
            &metrics,
        );
        assert!(!r.passed);
        assert!(
            r.details[1].contains("1 of 2 data points failed"),
            "{:?}",
            r.details
        );
        assert!(
            r.details[1].contains("data point #1 of \"http.server.request.count\""),
            "{:?}",
            r.details
        );
    }

    #[test]
    fn lists_at_most_five_failures() {
        let c: Vec<Captured> = (0..7).map(|_| captured("debug", login_span())).collect();
        let d = fails("{attribute: password, absent: true}", &c);
        assert!(d.contains("7 of 7 spans failed"));
        assert_eq!(d.matches("POST /login").count(), 5);
        assert!(d.contains("… and 2 more"));
    }

    #[test]
    fn value_rendering() {
        use opentelemetry_proto::tonic::common::v1::{ArrayValue, KeyValueList};
        let s = |v| show(&AnyValue { value: Some(v) });
        assert_eq!(s(Any::StringValue("a\"b".into())), "\"a\\\"b\"");
        assert_eq!(s(Any::BytesValue(vec![1, 2])), "<2 bytes>");
        assert_eq!(
            s(Any::ArrayValue(ArrayValue {
                values: vec![
                    AnyValue {
                        value: Some(Any::IntValue(1))
                    },
                    AnyValue { value: None }
                ]
            })),
            "[1, <empty>]"
        );
        assert_eq!(
            s(Any::KvlistValue(KeyValueList {
                values: vec![KeyValue {
                    key: "k".into(),
                    value: Some(AnyValue {
                        value: Some(Any::BoolValue(true))
                    }),
                    ..Default::default()
                }]
            })),
            "{k: true}"
        );
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    fn metrics() -> Payload {
        Payload::Metrics(fixture::<ExportMetricsServiceRequest>(
            "metrics/http-requests.json",
        ))
    }

    #[test]
    fn cross_signal_wording() {
        let got = [
            captured("otlp_http/traces", login_span()),
            captured("otlp_http/metrics", metrics()),
        ];
        let exporters = ExporterSignals::from([
            ("otlp_http/traces".into(), BTreeSet::from([Signal::Traces])),
            (
                "otlp_http/metrics".into(),
                BTreeSet::from([Signal::Metrics]),
            ),
            ("otlp_http/idle".into(), BTreeSet::from([Signal::Metrics])),
        ]);
        let ev = |yaml: &str| evaluate(&assertion(yaml), Signal::Traces, &exporters, &got);

        let r = ev("{exporter: otlp_http/metrics, count: 5}");
        assert_eq!(r.details[0], "Expected: 5 data points");
        assert_eq!(r.details[1], "Actual:   2 data points");
        assert_eq!(
            r.details[3],
            "Received: otlp_http/metrics: 2 data points, otlp_http/traces: 1 span"
        );
        let r = ev("{exporter: otlp_http/idle}");
        assert_eq!(r.details[0], "Expected: at least 1 data point");
        let r = ev("{dropped: true}");
        assert_eq!(r.details[1], "Actual:   3 items exported");
        let r = ev("{attribute: password, exporter: otlp_http/metrics, present: true}");
        assert!(
            r.details[0].ends_with("on every data point at otlp_http/metrics"),
            "{r:?}"
        );

        let mixed = [
            captured("otlp_http/both", login_span()),
            captured("otlp_http/both", metrics()),
            captured("otlp_http/both", Payload::Metrics(Default::default())),
        ];
        let r = evaluate(
            &assertion("{exporter: otlp_http/both, count: 1}"),
            Signal::Traces,
            &ExporterSignals::new(),
            &mixed,
        );
        assert_eq!(r.details[1], "Actual:   3 items");
        assert_eq!(
            r.details[3],
            "Received: otlp_http/both: 1 span + 2 data points"
        );
    }
}
