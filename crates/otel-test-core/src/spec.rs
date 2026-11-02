use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;

use crate::duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const MIN_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct TestSuite {
    pub source: PathBuf,
    pub collector_config: PathBuf,
    pub tests: Vec<TestCase>,
}

#[derive(Debug, Clone)]
pub struct TestCase {
    pub name: String,
    pub input: PathBuf,
    pub signal: Option<Signal>,
    pub receiver: Option<String>,
    pub timeout: Duration,
    pub expect: Vec<Assertion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Signal::Traces => "traces",
            Signal::Metrics => "metrics",
            Signal::Logs => "logs",
        })
    }
}

#[derive(Debug, Clone)]
pub enum Assertion {
    Attribute(AttributeAssertion),
    Dropped,
    ExporterReached { exporter: String },
    ExporterCount { exporter: String, count: u64 },
}

#[derive(Debug, Clone)]
pub struct AttributeAssertion {
    pub key: String,
    pub level: AttributeLevel,
    pub exporter: Option<String>,
    pub check: AttributeCheck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttributeLevel {
    #[default]
    Any,
    Resource,
    Scope,
    Item,
}

#[derive(Debug, Clone)]
pub enum AttributeCheck {
    Present,
    Absent,
    Equals(AttrValue),
    Matches { pattern: String, regex: Regex },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    String(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}

impl fmt::Display for AttrValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttrValue::String(s) => write!(f, "{s:?}"),
            AttrValue::Int(i) => write!(f, "{i}"),
            AttrValue::Double(d) => write!(f, "{d}"),
            AttrValue::Bool(b) => write!(f, "{b}"),
        }
    }
}

impl fmt::Display for Assertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Assertion::Dropped => f.write_str("telemetry is dropped"),
            Assertion::ExporterReached { exporter } => write!(f, "exporter {exporter} is reached"),
            Assertion::ExporterCount { exporter, count } => {
                write!(f, "exporter {exporter} receives {count} item(s)")
            }
            Assertion::Attribute(a) => {
                write!(f, "attribute {}", a.key)?;
                match a.level {
                    AttributeLevel::Any => {}
                    AttributeLevel::Resource => f.write_str(" (resource level)")?,
                    AttributeLevel::Scope => f.write_str(" (scope level)")?,
                    AttributeLevel::Item => f.write_str(" (item level)")?,
                }
                match &a.check {
                    AttributeCheck::Present => f.write_str(" is present")?,
                    AttributeCheck::Absent => f.write_str(" is absent")?,
                    AttributeCheck::Equals(v) => write!(f, " equals {v}")?,
                    AttributeCheck::Matches { pattern, .. } => write!(f, " matches /{pattern}/")?,
                }
                if let Some(e) = &a.exporter {
                    write!(f, " at exporter {e}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub location: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("failed to read test specification {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid test specification {}: {source}", path.display())]
    Syntax {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("invalid test specification {}:{}", path.display(), format_problems(problems))]
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

pub fn load(path: &Path) -> Result<TestSuite, SpecError> {
    let text = std::fs::read_to_string(path).map_err(|source| SpecError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let suite = parse(&text, path)?;

    let mut problems = Vec::new();
    if !suite.collector_config.is_file() {
        problems.push(Problem {
            location: "collector".into(),
            message: format!("file not found: {}", suite.collector_config.display()),
        });
    }
    for (i, test) in suite.tests.iter().enumerate() {
        if !test.input.is_file() {
            problems.push(Problem {
                location: format!("tests[{i}].input"),
                message: format!("file not found: {}", test.input.display()),
            });
        }
    }
    if problems.is_empty() {
        Ok(suite)
    } else {
        Err(SpecError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}

pub fn parse(text: &str, source: &Path) -> Result<TestSuite, SpecError> {
    if text.trim().is_empty() {
        return Err(SpecError::Invalid {
            path: source.to_path_buf(),
            problems: vec![Problem {
                location: "<root>".into(),
                message: "file is empty; expected `collector:` and `tests:`".into(),
            }],
        });
    }
    let raw: RawSuite = serde_yaml::from_str(text).map_err(|source_err| SpecError::Syntax {
        path: source.to_path_buf(),
        source: source_err,
    })?;
    let base = source.parent().unwrap_or(Path::new(""));
    let mut problems = Vec::new();
    let suite = convert_suite(raw, source, base, &mut problems);
    if problems.is_empty() {
        Ok(suite)
    } else {
        Err(SpecError::Invalid {
            path: source.to_path_buf(),
            problems,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSuite {
    collector: String,
    #[serde(default)]
    defaults: RawDefaults,
    tests: Vec<RawTest>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    timeout: Option<serde_yaml::Value>,
    receiver: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTest {
    name: String,
    input: String,
    signal: Option<Signal>,
    receiver: Option<String>,
    timeout: Option<serde_yaml::Value>,
    expect: Vec<RawAssertion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAssertion {
    attribute: Option<String>,
    level: Option<AttributeLevel>,
    present: Option<bool>,
    absent: Option<bool>,
    #[serde(default, deserialize_with = "explicit_value")]
    equals: Option<serde_yaml::Value>,
    matches: Option<String>,
    dropped: Option<bool>,
    exporter: Option<String>,
    count: Option<u64>,
}

fn explicit_value<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<serde_yaml::Value>, D::Error> {
    serde_yaml::Value::deserialize(d).map(Some)
}

fn convert_suite(
    raw: RawSuite,
    source: &Path,
    base: &Path,
    problems: &mut Vec<Problem>,
) -> TestSuite {
    let mut push = |location: String, message: String| problems.push(Problem { location, message });

    if raw.collector.trim().is_empty() {
        push("collector".into(), "must not be empty".into());
    }
    let default_timeout = match &raw.defaults.timeout {
        Some(v) => convert_timeout(v).unwrap_or_else(|m| {
            push("defaults.timeout".into(), m);
            DEFAULT_TIMEOUT
        }),
        None => DEFAULT_TIMEOUT,
    };
    if raw
        .defaults
        .receiver
        .as_deref()
        .is_some_and(|r| r.trim().is_empty())
    {
        push("defaults.receiver".into(), "must not be empty".into());
    }
    if raw.tests.is_empty() {
        push("tests".into(), "at least one test is required".into());
    }

    let mut names = HashSet::new();
    let mut tests = Vec::with_capacity(raw.tests.len());
    for (i, t) in raw.tests.into_iter().enumerate() {
        let loc = format!("tests[{i}]");
        if t.name.trim().is_empty() {
            push(format!("{loc}.name"), "must not be empty".into());
        } else if !names.insert(t.name.clone()) {
            push(
                format!("{loc}.name"),
                format!("duplicate test name {:?}", t.name),
            );
        }
        if t.input.trim().is_empty() {
            push(format!("{loc}.input"), "must not be empty".into());
        }
        if t.receiver.as_deref().is_some_and(|r| r.trim().is_empty()) {
            push(format!("{loc}.receiver"), "must not be empty".into());
        }
        let timeout = match &t.timeout {
            Some(v) => convert_timeout(v).unwrap_or_else(|m| {
                push(format!("{loc}.timeout"), m);
                default_timeout
            }),
            None => default_timeout,
        };
        if t.expect.is_empty() {
            push(
                format!("{loc}.expect"),
                "at least one assertion is required; a test without expectations would always pass"
                    .into(),
            );
        }
        let mut expect = Vec::with_capacity(t.expect.len());
        for (j, a) in t.expect.into_iter().enumerate() {
            let aloc = format!("{loc}.expect[{j}]");
            match convert_assertion(a) {
                Ok(a) => expect.push(a),
                Err((field, message)) => {
                    let location = match field {
                        Some(f) => format!("{aloc}.{f}"),
                        None => aloc,
                    };
                    push(location, message);
                }
            }
        }
        tests.push(TestCase {
            name: t.name,
            input: resolve(base, &t.input),
            signal: t.signal,
            receiver: t.receiver.or_else(|| raw.defaults.receiver.clone()),
            timeout,
            expect,
        });
    }

    TestSuite {
        source: source.to_path_buf(),
        collector_config: resolve(base, &raw.collector),
        tests,
    }
}

fn resolve(base: &Path, relative: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    out.extend(
        Path::new(relative)
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir)),
    );
    out
}

fn convert_timeout(value: &serde_yaml::Value) -> Result<Duration, String> {
    let d = match value {
        serde_yaml::Value::String(s) => duration::parse(s)?,
        serde_yaml::Value::Number(n) => {
            return Err(format!("{n} has no unit; write e.g. \"{n}s\" or \"{n}ms\""));
        }
        _ => return Err("expected a duration string such as \"3s\"".into()),
    };
    if d < MIN_TIMEOUT || d > MAX_TIMEOUT {
        return Err(format!(
            "must be between {}ms and {}s",
            MIN_TIMEOUT.as_millis(),
            MAX_TIMEOUT.as_secs()
        ));
    }
    Ok(d)
}

type AssertionError = (Option<&'static str>, String);

fn convert_assertion(raw: RawAssertion) -> Result<Assertion, AssertionError> {
    let exporter = match raw.exporter {
        Some(e) if e.trim().is_empty() => {
            return Err((Some("exporter"), "must not be empty".into()));
        }
        other => other,
    };

    let Some(key) = raw.attribute else {
        let stray = [
            ("level", raw.level.is_some()),
            ("present", raw.present.is_some()),
            ("absent", raw.absent.is_some()),
            ("equals", raw.equals.is_some()),
            ("matches", raw.matches.is_some()),
        ];
        if let Some((field, _)) = stray.iter().find(|(_, set)| *set) {
            return Err((
                Some(field),
                "requires `attribute:` in the same assertion".into(),
            ));
        }

        return match (raw.dropped, exporter, raw.count) {
            (Some(true), None, None) => Ok(Assertion::Dropped),
            (Some(false), _, _) => Err((
                Some("dropped"),
                "`dropped: false` is not supported; assert where telemetry should arrive with `exporter:`".into(),
            )),
            (Some(true), Some(e), _) => Err((
                Some("dropped"),
                format!(
                    "`dropped` applies to all exporters; to assert that only {e} received nothing use `exporter: {e}` with `count: 0`"
                ),
            )),
            (Some(true), None, Some(_)) => {
                Err((Some("count"), "cannot be combined with `dropped`; `count` requires `exporter:`".into()))
            }
            (None, Some(exporter), Some(count)) => Ok(Assertion::ExporterCount { exporter, count }),
            (None, Some(exporter), None) => Ok(Assertion::ExporterReached { exporter }),
            (None, None, Some(_)) => Err((Some("count"), "requires `exporter:` in the same assertion".into())),
            (None, None, None) => Err((
                None,
                "empty assertion: expected one of `attribute`, `dropped` or `exporter`".into(),
            )),
        };
    };

    if key.trim().is_empty() {
        return Err((Some("attribute"), "must not be empty".into()));
    }
    if raw.dropped.is_some() {
        return Err((
            Some("dropped"),
            "cannot be combined with `attribute`".into(),
        ));
    }
    if raw.count.is_some() {
        return Err((
            Some("count"),
            "cannot be combined with `attribute`; use a separate `exporter:` assertion".into(),
        ));
    }

    let given: Vec<&str> = [
        ("present", raw.present.is_some()),
        ("absent", raw.absent.is_some()),
        ("equals", raw.equals.is_some()),
        ("matches", raw.matches.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, set)| set.then_some(name))
    .collect();
    if given.len() != 1 {
        let found = if given.is_empty() {
            String::new()
        } else {
            format!(" (found {})", given.join(", "))
        };
        return Err((
            None,
            format!(
                "attribute assertion needs exactly one of `present`, `absent`, `equals` or `matches`{found}"
            ),
        ));
    }

    let check = if let Some(present) = raw.present {
        if !present {
            return Err((
                Some("present"),
                "`present: false` is not supported; use `absent: true`".into(),
            ));
        }
        AttributeCheck::Present
    } else if let Some(absent) = raw.absent {
        if !absent {
            return Err((
                Some("absent"),
                "`absent: false` is not supported; use `present: true`".into(),
            ));
        }
        AttributeCheck::Absent
    } else if let Some(value) = raw.equals {
        AttributeCheck::Equals(convert_value(&value).map_err(|m| (Some("equals"), m))?)
    } else {
        let pattern = raw.matches.expect("exactly one check is set");
        let regex = Regex::new(&format!("^(?:{pattern})$"))
            .map_err(|e| (Some("matches"), format!("invalid regular expression: {e}")))?;
        AttributeCheck::Matches { pattern, regex }
    };

    Ok(Assertion::Attribute(AttributeAssertion {
        key,
        level: raw.level.unwrap_or_default(),
        exporter,
        check,
    }))
}

fn convert_value(value: &serde_yaml::Value) -> Result<AttrValue, String> {
    use serde_yaml::Value;
    match value {
        Value::String(s) => Ok(AttrValue::String(s.clone())),
        Value::Bool(b) => Ok(AttrValue::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(AttrValue::Int(i))
            } else if n.is_u64() {
                Err(format!(
                    "{n} is out of range for an OTLP int attribute (max {})",
                    i64::MAX
                ))
            } else {
                Ok(AttrValue::Double(
                    n.as_f64().expect("non-integer YAML numbers are f64"),
                ))
            }
        }
        Value::Null => {
            Err("must not be null; to assert a missing attribute use `absent: true`".into())
        }
        Value::Sequence(_) | Value::Mapping(_) => {
            Err("only scalar values (string, integer, float, bool) are supported".into())
        }
        Value::Tagged(t) => Err(format!("YAML tags ({}) are not supported", t.tag)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(yaml: &str) -> TestSuite {
        match parse(yaml, Path::new("/work/otel-test.yaml")) {
            Ok(s) => s,
            Err(e) => panic!("expected spec to parse, got: {e}"),
        }
    }

    fn problems(yaml: &str) -> Vec<Problem> {
        match parse(yaml, Path::new("/work/otel-test.yaml")) {
            Err(SpecError::Invalid { problems, .. }) => problems,
            Err(e) => panic!("expected semantic problems, got: {e}"),
            Ok(_) => panic!("expected spec to be rejected"),
        }
    }

    fn syntax_error(yaml: &str) -> String {
        match parse(yaml, Path::new("/work/otel-test.yaml")) {
            Err(e @ SpecError::Syntax { .. }) => e.to_string(),
            Err(e) => panic!("expected a syntax error, got: {e}"),
            Ok(_) => panic!("expected spec to be rejected"),
        }
    }

    fn with_assertion(assertion: &str) -> String {
        format!(
            "collector: c.yaml\ntests:\n  - name: t\n    input: f.json\n    expect:\n      - {assertion}\n"
        )
    }

    fn single_assertion(assertion: &str) -> Assertion {
        parse_ok(&with_assertion(assertion)).tests[0].expect[0].clone()
    }

    fn single_problem(assertion: &str) -> Problem {
        let mut p = problems(&with_assertion(assertion));
        assert_eq!(p.len(), 1, "expected exactly one problem, got {p:?}");
        p.remove(0)
    }

    #[test]
    fn parses_example_from_readme() {
        let suite = parse_ok(
            r#"
collector: ./collector.yaml

tests:
  - name: passwords are redacted
    input: fixtures/login-log.json
    expect:
      - attribute: password
        absent: true

  - name: health checks are dropped
    input: fixtures/healthz-span.json
    expect:
      - dropped: true

  - name: errors reach datadog
    input: fixtures/error-span.json
    expect:
      - exporter: datadog
        count: 1
"#,
        );
        assert_eq!(suite.collector_config, Path::new("/work/collector.yaml"));
        assert_eq!(suite.tests.len(), 3);

        let t = &suite.tests[0];
        assert_eq!(t.name, "passwords are redacted");
        assert_eq!(t.input, Path::new("/work/fixtures/login-log.json"));
        assert_eq!(t.timeout, DEFAULT_TIMEOUT);
        assert_eq!(t.signal, None);
        assert!(matches!(
            &t.expect[0],
            Assertion::Attribute(AttributeAssertion { key, level: AttributeLevel::Any, exporter: None, check: AttributeCheck::Absent })
                if key == "password"
        ));
        assert!(matches!(suite.tests[1].expect[0], Assertion::Dropped));
        assert!(matches!(
            &suite.tests[2].expect[0],
            Assertion::ExporterCount { exporter, count: 1 } if exporter == "datadog"
        ));
    }

    #[test]
    fn parses_optional_fields_and_defaults() {
        let suite = parse_ok(
            r#"
collector: collector.yaml
defaults:
  timeout: 2s
  receiver: otlp
tests:
  - name: inherits
    input: a.json
    expect: [{dropped: true}]
  - name: overrides
    input: b.json
    signal: logs
    receiver: filelog/app
    timeout: 750ms
    expect: [{dropped: true}]
"#,
        );
        let [a, b] = &suite.tests[..] else { panic!() };
        assert_eq!(a.timeout, Duration::from_secs(2));
        assert_eq!(a.receiver.as_deref(), Some("otlp"));
        assert_eq!(b.timeout, Duration::from_millis(750));
        assert_eq!(b.receiver.as_deref(), Some("filelog/app"));
        assert_eq!(b.signal, Some(Signal::Logs));
    }

    #[test]
    fn parses_every_attribute_check() {
        assert!(matches!(
            single_assertion("{attribute: password, present: true}"),
            Assertion::Attribute(AttributeAssertion {
                check: AttributeCheck::Present,
                ..
            })
        ));
        assert!(matches!(
            single_assertion("{attribute: http.status_code, equals: 500}"),
            Assertion::Attribute(AttributeAssertion {
                check: AttributeCheck::Equals(AttrValue::Int(500)),
                ..
            })
        ));
        assert!(matches!(
            single_assertion("{attribute: ratio, equals: 0.5}"),
            Assertion::Attribute(AttributeAssertion { check: AttributeCheck::Equals(AttrValue::Double(d)), .. }) if d == 0.5
        ));
        assert!(matches!(
            single_assertion("{attribute: sampled, equals: true}"),
            Assertion::Attribute(AttributeAssertion {
                check: AttributeCheck::Equals(AttrValue::Bool(true)),
                ..
            })
        ));
        assert!(matches!(
            single_assertion("{attribute: code, equals: \"500\"}"),
            Assertion::Attribute(AttributeAssertion { check: AttributeCheck::Equals(AttrValue::String(s)), .. }) if s == "500"
        ));
    }

    #[test]
    fn regex_is_anchored() {
        let Assertion::Attribute(AttributeAssertion {
            check: AttributeCheck::Matches { regex, pattern },
            ..
        }) = single_assertion("{attribute: service.name, matches: \"payment.*\"}")
        else {
            panic!("expected matches assertion");
        };
        assert_eq!(pattern, "payment.*");
        assert!(regex.is_match("payment-api"));
        assert!(!regex.is_match("legacy-payment-api"));
        let Assertion::Attribute(AttributeAssertion {
            check: AttributeCheck::Matches { regex, .. },
            ..
        }) = single_assertion("{attribute: s, matches: \"a|b\"}")
        else {
            panic!();
        };
        assert!(regex.is_match("a") && regex.is_match("b") && !regex.is_match("ab"));
    }

    #[test]
    fn attribute_scope_fields() {
        let Assertion::Attribute(a) = single_assertion(
            "{attribute: service.name, level: resource, exporter: datadog, present: true}",
        ) else {
            panic!();
        };
        assert_eq!(a.level, AttributeLevel::Resource);
        assert_eq!(a.exporter.as_deref(), Some("datadog"));
    }

    #[test]
    fn exporter_reached_and_zero_count() {
        assert!(matches!(
            single_assertion("{exporter: debug}"),
            Assertion::ExporterReached { exporter } if exporter == "debug"
        ));
        assert!(matches!(
            single_assertion("{exporter: \"otlp_http/eu\", count: 0}"),
            Assertion::ExporterCount { exporter, count: 0 } if exporter == "otlp_http/eu"
        ));
    }

    #[test]
    fn rejects_ambiguous_or_meaningless_assertions() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "{attribute: password}",
                "tests[0].expect[0]",
                "exactly one of",
            ),
            (
                "{attribute: p, absent: true, present: true}",
                "tests[0].expect[0]",
                "found present, absent",
            ),
            (
                "{attribute: p, present: false}",
                "tests[0].expect[0].present",
                "use `absent: true`",
            ),
            (
                "{attribute: p, absent: false}",
                "tests[0].expect[0].absent",
                "use `present: true`",
            ),
            (
                "{attribute: \"\", absent: true}",
                "tests[0].expect[0].attribute",
                "must not be empty",
            ),
            (
                "{attribute: p, absent: true, dropped: true}",
                "tests[0].expect[0].dropped",
                "cannot be combined",
            ),
            (
                "{attribute: p, absent: true, count: 1}",
                "tests[0].expect[0].count",
                "cannot be combined",
            ),
            (
                "{attribute: p, equals: null}",
                "tests[0].expect[0].equals",
                "absent: true",
            ),
            (
                "{attribute: p, equals: [1, 2]}",
                "tests[0].expect[0].equals",
                "only scalar",
            ),
            (
                "{attribute: p, equals: 18446744073709551615}",
                "tests[0].expect[0].equals",
                "out of range",
            ),
            (
                "{attribute: p, matches: \"(\"}",
                "tests[0].expect[0].matches",
                "invalid regular expression",
            ),
            (
                "{dropped: false}",
                "tests[0].expect[0].dropped",
                "not supported",
            ),
            (
                "{dropped: true, exporter: datadog}",
                "tests[0].expect[0].dropped",
                "count: 0",
            ),
            (
                "{count: 1}",
                "tests[0].expect[0].count",
                "requires `exporter:`",
            ),
            (
                "{absent: true}",
                "tests[0].expect[0].absent",
                "requires `attribute:`",
            ),
            (
                "{level: resource, exporter: x}",
                "tests[0].expect[0].level",
                "requires `attribute:`",
            ),
            (
                "{exporter: \" \"}",
                "tests[0].expect[0].exporter",
                "must not be empty",
            ),
            ("{}", "tests[0].expect[0]", "empty assertion"),
        ];
        for (assertion, location, fragment) in cases {
            let p = single_problem(assertion);
            assert_eq!(&p.location, location, "for {assertion}");
            assert!(
                p.message.contains(fragment),
                "for {assertion}: {:?} lacks {fragment:?}",
                p.message
            );
        }
    }

    #[test]
    fn collects_all_problems_with_locations() {
        let p = problems(
            r#"
collector: ""
defaults:
  timeout: 5
tests:
  - name: same
    input: a.json
    timeout: 1h
    expect: []
  - name: same
    input: ""
    receiver: ""
    expect:
      - dropped: true
      - count: 2
"#,
        );
        let got: Vec<(&str, &str)> = p
            .iter()
            .map(|p| (p.location.as_str(), p.message.as_str()))
            .collect();
        let expected_locations = [
            "collector",
            "defaults.timeout",
            "tests[0].timeout",
            "tests[0].expect",
            "tests[1].name",
            "tests[1].input",
            "tests[1].receiver",
            "tests[1].expect[1].count",
        ];
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            expected_locations,
            "{got:#?}"
        );
        assert!(got[1].1.contains("has no unit"));
        assert!(got[2].1.contains("unknown unit"));
        assert!(got[4].1.contains("duplicate test name"));
    }

    #[test]
    fn timeout_bounds() {
        let spec = |t: &str| {
            format!(
                "collector: c\ntests:\n  - {{name: n, input: i, timeout: {t}, expect: [{{dropped: true}}]}}\n"
            )
        };
        assert!(
            problems(&spec("50ms"))[0]
                .message
                .contains("between 100ms and 600s")
        );
        assert!(problems(&spec("11m"))[0].message.contains("between"));
        assert!(
            problems(&spec("[1]"))[0]
                .message
                .contains("expected a duration")
        );
        assert_eq!(
            parse_ok(&spec("10m")).tests[0].timeout,
            Duration::from_secs(600)
        );
    }

    #[test]
    fn requires_tests() {
        let p = problems("collector: c.yaml\ntests: []\n");
        assert_eq!(p[0].location, "tests");
    }

    #[test]
    fn empty_file() {
        let p = problems("  \n\t\n");
        assert_eq!(p[0].location, "<root>");
        assert!(p[0].message.contains("file is empty"));
        assert!(syntax_error("# only a comment\n").contains("collector"));
    }

    #[test]
    fn syntax_errors_carry_field_path_and_position() {
        let e = syntax_error(
            "collector: c\ntests:\n  - name: t\n    input: i\n    expect: [{dropped: true}]\n    timout: 3s\n",
        );
        assert!(e.contains("unknown field `timout`"), "{e}");
        assert!(e.contains("tests[0]"), "{e}");
        assert!(e.contains("line 6"), "{e}");

        let e = syntax_error("collector: c\ntests:\n  - name: t\n    expect: []\n");
        assert!(e.contains("missing field `input`"), "{e}");

        let e = syntax_error(
            "collector: c\ntests:\n  - name: t\n    input: i\n    signal: trace\n    expect: []\n",
        );
        assert!(e.contains("unknown variant `trace`"), "{e}");
        assert!(e.contains("`traces`"), "{e}");

        let e = syntax_error(
            "collector: c\ntests:\n  - name: t\n    input: i\n    expect:\n      - exporter: d\n        count: -1\n",
        );
        assert!(e.contains("count"), "{e}");

        let e = syntax_error("collector: a\ncollector: b\ntests: []\n");
        assert!(e.contains("duplicate"), "{e}");
    }

    #[test]
    fn assertion_display() {
        let s = |a: &str| single_assertion(a).to_string();
        assert_eq!(
            s("{attribute: password, absent: true}"),
            "attribute password is absent"
        );
        assert_eq!(
            s("{attribute: service.name, level: resource, matches: \"pay.*\", exporter: dd}"),
            "attribute service.name (resource level) matches /pay.*/ at exporter dd"
        );
        assert_eq!(
            s("{attribute: code, equals: \"500\"}"),
            "attribute code equals \"500\""
        );
        assert_eq!(
            s("{exporter: dd, count: 2}"),
            "exporter dd receives 2 item(s)"
        );
        assert_eq!(s("{dropped: true}"), "telemetry is dropped");
    }

    #[test]
    fn load_reports_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("otel-test.yaml");
        std::fs::write(&spec, with_assertion("{dropped: true}")).unwrap();

        let Err(SpecError::Invalid { problems, .. }) = load(&spec) else {
            panic!("expected missing files to be reported");
        };
        assert_eq!(
            problems
                .iter()
                .map(|p| p.location.as_str())
                .collect::<Vec<_>>(),
            ["collector", "tests[0].input"]
        );

        std::fs::write(dir.path().join("c.yaml"), "").unwrap();
        std::fs::write(dir.path().join("f.json"), "{}").unwrap();
        let suite = load(&spec).expect("all files exist now");
        assert_eq!(suite.collector_config, dir.path().join("c.yaml"));

        let missing = dir.path().join("nope.yaml");
        assert!(matches!(load(&missing), Err(SpecError::Read { .. })));
    }
}
