use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn otel_test(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_otel-test"))
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("failed to run otel-test")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn validate_accepts_example() {
    let o = otel_test(&[
        "validate",
        "-f",
        "examples/password-redaction/otel-test.yaml",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("Tests:     3"), "{out}");
    assert!(out.contains("passwords are removed from spans"), "{out}");
    assert!(out.contains("attribute password is absent"), "{out}");
    assert!(out.contains("(1 span via otlp, timeout 2000ms)"), "{out}");
    assert!(
        out.contains("✓ all fixtures, receivers and exporter references resolve"),
        "{out}"
    );
    assert!(out.contains("✓ test specification is valid"), "{out}");
    assert!(out.contains("Collector: v"), "{out}");
    assert!(
        out.contains("receiver otlp replaced by otlp/otel_test.otlp"),
        "{out}"
    );
    assert!(
        out.contains("✓ rewritten Collector configuration passes `otelcol validate`"),
        "{out}"
    );
}

#[test]
fn validate_reports_every_problem_with_exit_code_2() {
    let o = otel_test(&["validate", "-f", "tests/specs/invalid.yaml"]);
    assert_eq!(o.status.code(), Some(2));
    let err = stderr(&o);
    assert!(
        err.contains("tests[0].expect: at least one assertion"),
        "{err}"
    );
    assert!(
        err.contains("tests[1].expect[0]: attribute assertion needs exactly one of"),
        "{err}"
    );
    assert!(!err.contains("missing.json"), "{err}");
}

#[test]
fn validate_reports_missing_fixture() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("collector.yaml"), "receivers: {}\n").unwrap();
    let spec = dir.path().join("otel-test.yaml");
    std::fs::write(
        &spec,
        "collector: collector.yaml\ntests:\n  - name: t\n    input: gone.json\n    expect: [{dropped: true}]\n",
    )
    .unwrap();
    let o = otel_test(&["validate", "-f", spec.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    let err = stderr(&o);
    assert!(err.contains("tests[0].input: file not found"), "{err}");
    assert!(err.contains("gone.json"), "{err}");
}

#[test]
fn validate_reports_yaml_position() {
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("otel-test.yaml");
    std::fs::write(
        &spec,
        "collector: c.yaml\ntests:\n  - name: t\n    inptu: f.json\n",
    )
    .unwrap();
    let o = otel_test(&["validate", "-f", spec.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    let err = stderr(&o);
    assert!(err.contains("unknown field `inptu`"), "{err}");
    assert!(err.contains("line 4"), "{err}");
}

#[test]
fn missing_spec_file_is_a_spec_error() {
    let o = otel_test(&["validate", "-f", "does-not-exist.yaml"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("failed to read test specification"));
}

#[test]
fn filter_matching_nothing_fails() {
    let o = otel_test(&[
        "test",
        "-f",
        "examples/password-redaction/otel-test.yaml",
        "--filter",
        "no such test",
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("no tests match filter \"no such test\" (3 test(s) defined)"));
}

#[test]
fn filter_is_case_insensitive() {
    let o = otel_test(&[
        "test",
        "-f",
        "examples/password-redaction/otel-test.yaml",
        "--filter",
        "PASSWORDS",
    ]);
    assert!(stdout(&o).contains("Tests:     2"), "{}", stdout(&o));
}

#[test]
fn example_suite_passes_with_exit_code_0() {
    let o = otel_test(&["test", "-f", "examples/password-redaction/otel-test.yaml"]);
    assert_eq!(o.status.code(), Some(0), "{}\n{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    for line in [
        "Collector: v0.164.0\n",
        "✓ passwords are removed from spans\n",
        "✓ passwords are removed from logs\n",
        "✓ health checks are dropped\n",
        "\n3 passed, 0 failed\n",
        "\nTime: ",
    ] {
        assert!(out.contains(line), "missing {line:?} in:\n{out}");
    }
}

#[test]
fn verbose_shows_rewrite_notes_and_every_assertion() {
    let o = otel_test(&[
        "test",
        "-v",
        "-f",
        "examples/password-redaction/otel-test.yaml",
    ]);
    let out = stdout(&o);
    assert!(out.contains("Configuration rewrite:"), "{out}");
    assert!(out.contains("    ✓ attribute password is absent"), "{out}");
    assert!(
        out.contains("    ✓ exporter otlp_http/backend receives 1 item(s)"),
        "{out}"
    );
    assert!(!out.contains("arrived during Collector shutdown"), "{out}");
    assert!(
        out.contains("✓ health checks are dropped ("),
        "verbose shows durations: {out}"
    );
}

#[test]
fn failing_assertion_is_exit_code_1_with_expected_and_actual() {
    let dir = suite_with_config(
        "receivers: {otlp: }\nexporters: {debug: }\nservice: {pipelines: {traces: {receivers: [otlp], exporters: [debug]}}}\n",
    );
    let o = otel_test(&["test", "-f", &spec_arg(&dir)]);
    assert_eq!(o.status.code(), Some(1), "{}\n{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("✗ t\n"), "{out}");
    assert!(
        out.contains("    ✗ telemetry is dropped\n      Expected: nothing exported\n      Actual:   1 span exported\n      Received: debug: 1 span\n"),
        "{out}"
    );
    assert!(out.contains("0 passed, 1 failed"), "{out}");
    assert!(stderr(&o).contains("1 test(s) failed"));
}

#[test]
fn spec_config_mismatch_is_exit_code_2() {
    let dir = suite_with_config(
        "receivers: {otlp: }\nexporters: {debug: }\nservice: {pipelines: {traces: {receivers: [otlp], exporters: [debug]}}}\n",
    );
    let spec = dir.path().join("otel-test.yaml");
    std::fs::write(
        &spec,
        "collector: collector.yaml\ntests:\n  - {name: t, input: f.json, expect: [{exporter: datadog, count: 1}]}\n",
    )
    .unwrap();
    let o = otel_test(&["test", "-f", &spec_arg(&dir)]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("does not match the Collector configuration"),
        "{err}"
    );
    assert!(err.contains("test \"t\".expect[0]: exporter \"datadog\" is not used by any pipeline (exporters: debug)"), "{err}");
}

fn suite_with_config(collector_yaml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("collector.yaml"), collector_yaml).unwrap();
    std::fs::copy(
        repo_root().join("fixtures/traces/app-span.json"),
        dir.path().join("f.json"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("otel-test.yaml"),
        "collector: collector.yaml\ntests:\n  - {name: t, input: f.json, expect: [{dropped: true}]}\n",
    )
    .unwrap();
    dir
}

fn spec_arg(dir: &tempfile::TempDir) -> String {
    dir.path()
        .join("otel-test.yaml")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn collector_rejecting_config_is_exit_code_3() {
    let dir = suite_with_config(
        "receivers: {otlp: }\nprocessors: {batch: {timout: 1s}}\nexporters: {debug: }\nservice: {pipelines: {traces: {receivers: [otlp], processors: [batch], exporters: [debug]}}}\n",
    );
    let o = otel_test(&["validate", "-f", &spec_arg(&dir)]);
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("the Collector rejected the rewritten configuration"),
        "{err}"
    );
    assert!(err.contains("invalid keys: timout"), "{err}");
}

#[test]
fn unanalysable_config_is_exit_code_3() {
    let dir = suite_with_config(
        "receivers: {otlp: }\nexporters: {debug: }\nservice: {pipelines: {traces: {receivers: [otlp], processors: [batch], exporters: [debug]}}}\n",
    );
    let o = otel_test(&["validate", "-f", &spec_arg(&dir)]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        stderr(&o).contains("service.pipelines.traces.processors: \"batch\" is not defined"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn missing_collector_binary_is_exit_code_4() {
    let o = otel_test(&[
        "validate",
        "-f",
        "examples/password-redaction/otel-test.yaml",
        "--collector-bin",
        "/nonexistent/otelcol-contrib",
    ]);
    assert_eq!(o.status.code(), Some(4));
    assert!(stderr(&o).contains("Collector binary not found: /nonexistent/otelcol-contrib"));
}

#[test]
fn offline_without_cache_is_exit_code_4() {
    let empty = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_otel-test"))
        .args([
            "validate",
            "--offline",
            "-f",
            "examples/password-redaction/otel-test.yaml",
        ])
        .current_dir(repo_root())
        .env("OTEL_TEST_CACHE_DIR", empty.path())
        .env_remove("OTEL_TEST_COLLECTOR_BIN")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(4));
    assert!(
        stderr(&o).contains("--offline forbids downloading"),
        "{}",
        stderr(&o)
    );
}

fn collector_path() -> PathBuf {
    let o = otel_test(&[
        "validate",
        "-f",
        "examples/password-redaction/otel-test.yaml",
    ]);
    let out = stdout(&o);
    let path = out
        .lines()
        .find_map(|l| l.strip_prefix("Collector: v0.164.0 ("))
        .and_then(|l| l.strip_suffix(')'))
        .unwrap_or_else(|| panic!("validate prints the Collector path:\n{out}\n{}", stderr(&o)));
    repo_root().join(path)
}

#[cfg(unix)]
#[test]
fn relative_collector_bin_path() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(collector_path(), dir.path().join("otelcol-contrib")).unwrap();
    let spec = repo_root().join("examples/password-redaction/otel-test.yaml");
    let o = Command::new(env!("CARGO_BIN_EXE_otel-test"))
        .args(["test", "--collector-bin", "./otelcol-contrib", "-f"])
        .arg(&spec)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}\n{}", stdout(&o), stderr(&o));
}

#[test]
fn init_creates_a_passing_project() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("proj");
    let o = otel_test(&["init", target.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    for f in [
        "otel-test.yaml",
        "collector.yaml",
        "fixtures/login-span.json",
        "fixtures/login-log.json",
        "fixtures/healthz-span.json",
    ] {
        assert!(target.join(f).is_file(), "{f} missing");
    }
    assert!(
        stdout(&o).contains("Next: run `otel-test test -f "),
        "{}",
        stdout(&o)
    );

    let o = Command::new(env!("CARGO_BIN_EXE_otel-test"))
        .arg("test")
        .current_dir(&target)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}\n{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("3 passed, 0 failed"));

    let o = otel_test(&["init", target.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("refusing to overwrite existing files (use --force)"));
    assert_eq!(
        otel_test(&["init", "--force", target.to_str().unwrap()])
            .status
            .code(),
        Some(0)
    );
}

#[test]
fn color_flag_controls_ansi_output() {
    let args = [
        "test",
        "-f",
        "examples/password-redaction/otel-test.yaml",
        "--filter",
        "health",
    ];
    let plain = otel_test(&args);
    assert!(
        !stdout(&plain).contains('\u{1b}'),
        "piped output must not be colored by default"
    );

    let mut colored: Vec<&str> = args.to_vec();
    colored.extend(["--color", "always"]);
    let o = otel_test(&colored);
    assert!(
        stdout(&o).contains("\u{1b}[32m"),
        "green check mark expected: {:?}",
        stdout(&o)
    );

    let o = Command::new(env!("CARGO_BIN_EXE_otel-test"))
        .args(args)
        .current_dir(repo_root())
        .env("CLICOLOR_FORCE", "1")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(!stdout(&o).contains('\u{1b}'), "NO_COLOR wins");
}

#[cfg(unix)]
#[test]
fn interrupt_kills_the_collector() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let unique = format!("otelcol-sigint-{}", std::process::id());
    let bin = dir.path().join(&unique);
    std::os::unix::fs::symlink(collector_path(), &bin).unwrap();
    std::fs::write(
        dir.path().join("collector.yaml"),
        "receivers: {otlp: }\nexporters: {debug: }\nservice: {pipelines: {traces: {receivers: [otlp], exporters: [debug]}}}\n",
    )
    .unwrap();
    std::fs::copy(
        repo_root().join("fixtures/traces/app-span.json"),
        dir.path().join("f.json"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("otel-test.yaml"),
        "collector: collector.yaml\ntests:\n  - {name: slow, input: f.json, timeout: 60s, expect: [{dropped: true}]}\n",
    )
    .unwrap();

    let running = |pattern: &str| {
        Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    let pattern = format!("{unique} --config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_otel-test"))
        .args(["test", "--collector-bin"])
        .arg(&bin)
        .current_dir(dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !running(&pattern) {
        assert!(Instant::now() < deadline, "the Collector never started");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    let pid = child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap()
            .success()
    );

    let status = child.wait().unwrap();
    let mut err = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
    assert_eq!(status.code(), Some(130), "{err}");
    assert!(err.contains("error: interrupted"), "{err}");

    let deadline = Instant::now() + Duration::from_secs(5);
    while running(&pattern) {
        assert!(Instant::now() < deadline, "the Collector survived Ctrl-C");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn unknown_flag_is_usage_error() {
    let o = otel_test(&["test", "--bogus"]);
    assert_eq!(o.status.code(), Some(2));
}
