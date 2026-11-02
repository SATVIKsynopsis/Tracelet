# otel-test

**Unit tests for OpenTelemetry Collector configurations.**

otel-test runs your *real* `otelcol-contrib` binary with your *real*
processors and connectors, injects telemetry fixtures, captures what every
exporter would have sent, and checks it against declarative expectations.

```yaml
collector: ./collector.yaml

tests:
  - name: passwords are redacted
    input: fixtures/login-span.json
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
```

```console
$ otel-test test
otel-test v0.1.0

Collector: v0.164.0
Config:    collector.yaml
Tests:     3

✓ passwords are redacted
✓ health checks are dropped
✗ errors reach datadog

    ✗ exporter datadog receives 1 item(s)
      Expected: 1 span
      Actual:   0 spans
      Exporter: datadog
      Received: otlp_http/backend: 1 span

2 passed, 1 failed

Time: 11.30s
error: 1 test(s) failed
```

(Real output, from a config whose routing condition sends errors to the wrong
exporter. Tests with `absent`, `dropped` or `count` observe their full timeout,
3 s by default.)

## Why

Collector configurations encode policy: what is redacted, what is sampled or
dropped, and where each signal goes. That policy is usually verified by
deploying and looking at a backend. Mistakes such as a processor in the wrong
order, an OTTL condition that never matches, or a routing table sending errors
to the wrong place show up as leaked credentials or missing data in production.

otel-test makes that policy testable in CI. It does not simulate the
Collector. It runs the Collector, so processors behave exactly as they do in
production.

## Installation

otel-test is not yet published to crates.io. Build it from source (Rust 1.88
or newer):

```bash
git clone <this repository>
cd otel-test
cargo install --path crates/otel-test-cli
```

This installs the `otel-test` binary. On first use it downloads the pinned
**otelcol-contrib v0.164.0** release for your platform (~100 MB), verifies it
against a SHA-256 checksum compiled into otel-test, and caches it. To use a
Collector you already have:

```bash
otel-test test --collector-bin /usr/local/bin/otelcol-contrib
# or
export OTEL_TEST_COLLECTOR_BIN=/usr/local/bin/otelcol-contrib
```

| Variable | Effect |
|----------|--------|
| `OTEL_TEST_COLLECTOR_BIN` | Collector binary to use (overridden by `--collector-bin`) |
| `OTEL_TEST_CACHE_DIR` | Cache directory (default `~/Library/Caches/otel-test`, `$XDG_CACHE_HOME/otel-test`, `%LOCALAPPDATA%\otel-test`) |
| `OTEL_TEST_DOWNLOAD_BASE_URL` | Mirror for the release download; checksums are still enforced |
| `NO_COLOR`, `CLICOLOR_FORCE` | Color control (also `--color auto\|always\|never`) |
| `RUST_LOG` | Internal diagnostics, e.g. `RUST_LOG=otel_test_core=debug` |

`--offline` forbids downloading.

## Quick start

```bash
mkdir my-collector-tests && cd my-collector-tests
otel-test init        # writes otel-test.yaml, collector.yaml and fixtures/
otel-test test        # runs the starter suite: 3 passing tests
```

For realistic configurations, with regressions they catch, see
[examples/real-world](examples/real-world): PII redaction, multi-tenant routing,
tail sampling, metrics transformation and log redaction.

The starter project is [examples/password-redaction](examples/password-redaction):
a Collector that deletes passwords, hashes user names and drops health checks,
with tests for each. Replace `collector.yaml` with your own config and adapt the
tests.

```bash
otel-test validate              # check spec, fixtures and rewritten config; run nothing
otel-test test --verbose        # every assertion, rewrite notes, Collector logs on failure
otel-test test --filter redact  # only tests whose name contains "redact"
otel-test test -f path/to/otel-test.yaml
```

## Test file reference

```yaml
collector: ./collector.yaml   # required; relative to this file

defaults:                     # optional
  timeout: 3s                 # observation window: 100ms–10m, default 3s
  receiver: otlp              # receiver to inject through, when ambiguous

tests:
  - name: unique name         # required; used by --filter
    input: fixtures/x.json    # required; OTLP/JSON fixture, relative to this file
    signal: traces            # optional: traces | metrics | logs (inferred from the fixture)
    receiver: otlp/internal   # optional: receiver ID from your config to inject through
    timeout: 5s               # optional; overrides defaults.timeout
    expect:                   # required; at least one assertion
      - ...
```

Durations are an integer with `ms`, `s` or `m`. Every problem in a spec file is
reported at once, with its location (`tests[1].expect[0].equals: …`).

### Assertions

| Assertion | Passes when |
|-----------|-------------|
| `{attribute: K, present: true}` | every captured item has attribute `K` |
| `{attribute: K, absent: true}` | no captured item has `K`, at item, scope or resource level |
| `{attribute: K, equals: V}` | every captured item has `K == V` |
| `{attribute: K, matches: RE}` | every captured value of `K` is a string matching `RE` **in full** (`^(?:RE)$`) |
| `{dropped: true}` | nothing reaches any exporter |
| `{exporter: E}` | at least one item reaches exporter `E` |
| `{exporter: E, count: N}` | exactly `N` items reach `E` (`count: 0` = never reaches it) |

*Items* are spans, log records or metric data points.

* `exporter: E` on an attribute assertion restricts it to what `E` received.
* `level: item | scope | resource | any` (default `any`) selects where
  attributes are looked up. `any` uses the first match in item → scope →
  resource order.
* `equals` is type-strict, as OTLP is: `500` (int) ≠ `"500"` (string) ≠
  `500.0` (double). Failure messages show the actual type.
* **Attribute assertions fail when nothing was captured.** A redaction test
  can not pass just because the telemetry never arrived. Combine with
  `dropped: true` in a separate test if dropping is intended.
* Exporter names are the IDs from your config (`datadog`, `otlp_http/backend`).
  Referencing an exporter no pipeline uses, or a connector, is a spec error.

### Fixtures

Fixtures are **standard OTLP/JSON**: the body an OTLP/HTTP client would POST to
`/v1/traces`, `/v1/logs` or `/v1/metrics`. They are injected byte for byte, so
the Collector's own receiver decodes them. See [fixtures/README.md](fixtures/README.md)
for the format rules (hex IDs, int64 as strings, …) and the bundled fixtures:
a normal span, a health check, an error span, a span and a log with
credentials, and a metric.

## How it works

For each run:

1. **Parse and plan.** The spec, the Collector config and every fixture are
   validated. Each test is resolved to an injection receiver, and every
   exporter it names is checked against the config.
2. **Rewrite the config.** Only the edges of the pipeline graph change:
   * every receiver used by a pipeline is replaced by a local OTLP/HTTP
     receiver, one per original receiver, so routing between pipelines is
     preserved;
   * every exporter is replaced by an OTLP/HTTP exporter pointing at otel-test's
     capture server, keeping its identity (`datadog` is still reported as
     `datadog`);
   * **processors, connectors, pipeline processor order and pipeline IDs are
     copied verbatim**;
   * extensions not referenced by a processor or connector are removed, and
     self-telemetry is redirected to stderr.

   Every change is listed by `otel-test validate`. The rewritten config is
   checked with `otelcol validate` before anything runs.
3. **Run each test in a fresh Collector.** Inject the fixture (the Collector
   must accept it), observe for the timeout, shut the Collector down gracefully,
   and evaluate the assertions on everything captured. Telemetry that `batch`
   or `tail_sampling` were still holding is flushed at shutdown and counted for
   the right test. Tests with `dropped`, `count` or `absent` always observe the
   full timeout; tests with only positive assertions finish once all of them
   hold and the output has settled.
4. **Report.** A Collector crash, a rejected fixture or a capture failure is an
   *error* (exit 5), never a pass.

[docs/architecture.md](docs/architecture.md) documents the rewriting rules,
the lifecycle, and the Collector behaviors they rely on, each verified against
v0.164.0.

### Supported signals

Traces, logs and metrics. Each is tested end to end through the real
Collector. Profiles pipelines are rejected.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | All tests passed |
| 1 | At least one assertion failed |
| 2 | Usage error, invalid spec or fixture, or spec does not match the config |
| 3 | Collector config invalid, unsupported, or rejected by `otelcol validate` |
| 4 | Collector binary unavailable, or the Collector failed to start |
| 5 | Execution error (fixture rejected, Collector crashed, capture failure) |
| 130 | Interrupted (Ctrl-C); the Collector is killed and temp files removed |

## Limitations

* **Receivers and exporters themselves are not tested.** Their configuration
  is replaced: receiver parsing (e.g. `filelog` operators, Prometheus scraping)
  and exporter behavior (e.g. Datadog translation, headers, queues) do not run.
  otel-test tests what happens *between* them.
* **Fixtures are injected over OTLP only**, one fixture per test, through the
  receiver you choose.
* **Assertions cover attributes and delivery**, not span names, status, log
  bodies or metric values.
* `${env:…}` references inside processors are resolved by the Collector from
  the environment otel-test runs in. Provider references in place of whole
  sections or lists (`exporters: ${file:x.yaml}`) are rejected.
* `groupbytrace` discards pending traces at shutdown. otel-test warns when its
  `wait_duration` is not shorter than a test's timeout.
* `span_metrics` and `service_graph` emit only on their flush timer, never at
  shutdown. Set `metrics_flush_interval` below the test timeout (otel-test
  warns otherwise), and avoid `count` on their output, which repeats.
* `k8s_attributes` does not start outside a Kubernetes cluster unless
  `passthrough: true` is set; otel-test reports a startup error with a hint.
  Cloud `resource_detection` detectors add their timeout to every test.
* Routing on request metadata (`context: request`, client headers) can not be
  tested: fixtures are injected without custom headers.
* `equals` must hold for every item. There is no "one of" check and no way to
  assert a value's type without its exact value.
* Suites run sequentially, with ≈200 ms per test for the Collector restart
  plus the observation window.
* Pinned to otelcol-contrib v0.164.0. Other builds work through
  `--collector-bin` if they include the `otlp` receiver and the `otlp_http` (or
  older `otlphttp`) exporter. The `health_check` extension is used for
  readiness when present.
* **Windows is untested.** It has no graceful stop signal, so the Collector is
  killed rather than shut down and buffered telemetry is not flushed.
* Docker-based execution is not implemented.

## Architecture

```text
crates/
  otel-test-cli/      CLI: commands, output, exit codes, `init` templates
  otel-test-core/     spec · config model · rewriting · binary download
                      · process lifecycle · fixtures · assertions · runner
  otel-test-capture/  OTLP/HTTP + OTLP/gRPC capture server, per-test windows
examples/             password-redaction: the `init` starter project
fixtures/             shared OTLP/JSON fixtures
tests/                scenarios used by the integration tests
docs/                 architecture and Collector compatibility notes
```

## Contributing

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The integration tests run the real Collector. They resolve it as the CLI does,
so the first run downloads ~100 MB into the cache. Set
`OTEL_TEST_COLLECTOR_BIN` to reuse an existing binary. Tests fail rather than
skip when no Collector is available.

Guidelines:

* Every behavior claim about the Collector needs evidence: an integration test
  or a *(verified)* note in `docs/architecture.md`.
* A test must never pass because something did not happen for an unrelated
  reason (startup failure, missing capture, rejected input).
* `examples/password-redaction` doubles as the `init` template in
  `crates/otel-test-cli/templates/`. A unit test keeps them identical, so edit
  both.

**Upgrading the pinned Collector:** update `PINNED_VERSION` and `CHECKSUMS`
in `crates/otel-test-core/src/binary.rs` from the release's `.sha256` assets,
bump the cache key in `.github/workflows/ci.yml`, re-check the *Compatibility
concerns* in `docs/architecture.md`, and run the full test suite.

## License

Apache-2.0
