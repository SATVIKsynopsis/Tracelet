# otel-test internal architecture

This document records the design decisions behind otel-test and the
Collector-compatibility facts they rest on. Facts marked *(verified)* were
checked against `otelcol-contrib` v0.164.0 (darwin/arm64) during development.

## Crates

```text
otel-test-cli      clap commands, human output, exit codes
   │
   ├── otel-test-core     spec parsing · collector config model · rewriting
   │                      · binary management · process lifecycle
   │                      · fixtures · assertions · orchestration
   │
   └── otel-test-capture  OTLP/gRPC + OTLP/HTTP capture server,
                          per-exporter, per-test in-memory store
```

`otel-test-core` depends on `otel-test-capture` (the orchestrator starts the
capture server and reads from its store). The capture crate knows nothing about
specs or the Collector; it only receives OTLP and files it under an exporter
identity.

### Modules in `otel-test-core`

| Module      | Responsibility |
|-------------|----------------|
| `spec`      | `otel-test.yaml` → validated `TestSuite` (all problems reported at once, with `tests[i].expect[j]` locations). |
| `config`    | Collector YAML → `CollectorConfig`: component IDs, pipelines, connector detection. Keeps the raw `serde_yaml::Value` tree for everything it does not need to understand. |
| `rewrite`   | `CollectorConfig` → rewritten YAML + `Rewritten` (injection receiver ports, exporter → capture slug map, notes). |
| `binary`    | Resolve `--collector-bin` / env / cache, or download the pinned Contrib release, verify SHA-256, cache it. |
| `collector` | Probe a binary (`--version`, `components`), run `validate`, start the child process, readiness probe, log capture, termination. |
| `session`   | Rewrite against a running capture server → register exporter slugs → write to a private temp dir → `otelcol validate` → start. |
| `fixture`   | Load OTLP/JSON fixtures, validate by decoding, infer the signal; injected verbatim. |
| `assert`    | Evaluate `Assertion`s against captured data; produce expected-vs-actual details. |
| `runner`    | Plan tests against the rewritten config; run each in a fresh Collector. |

## Rewriting strategy

The rewrite operates on the **raw user YAML** (after applying `<<` merge keys),
not on `otelcol print-config` output. Reasons:

* `print-config` expands every default (≈5× larger config) and, in unredacted
  mode, writes resolved secrets to disk. Feeding default-expanded output back is
  not guaranteed to round-trip. *(verified: output contains fields like
  `sending_queue.batch: null`, `sizer: {}`)*
* Leaving `${env:…}` references inside processors untouched means the Collector
  resolves them exactly as it would in production.

An overlay (`--config user.yaml --config overlay.yaml`) is **not** viable: the
Collector validates *every defined* component, even unused ones, and merging can
not delete keys. *(verified: an unused `datadog` exporter without `api.key`
fails validation)*. The rewriter therefore emits one complete config.

### What changes

| Section | Treatment |
|---------|-----------|
| `processors` | Copied verbatim. Pipeline `processors:` lists copied verbatim (order preserved). |
| `connectors` | Copied verbatim. A connector ID appearing in a pipeline's `receivers`/`exporters` is **kept** — it is internal routing, not an edge of the system. |
| `receivers` | Every non-connector receiver `R` referenced by a pipeline is replaced by `otlp/otel_test.R` listening on a unique `127.0.0.1` port. One injection receiver per original receiver preserves routing when different pipelines consume different receivers. A test picks the receiver with `receiver:`; when only one candidate exists for the fixture's signal it is chosen automatically. |
| `exporters` | Every non-connector exporter `E` referenced by a pipeline is replaced by `otlp_http/otel_test.E` (or `otlphttp/…` on Collectors that predate the rename) pointing at `http://127.0.0.1:<capture>/<slug(E)>`. Queueing and retries are disabled so delivery is prompt and failures are visible. The original identity `E` is retained in the plan, so captured data is reported under `E`. |
| unused receivers/exporters | Dropped (they would otherwise be validated with production credentials). Reported as a verbose-mode note. |
| `extensions` | An extension is kept only if a processor or connector references its ID (directly, or through another kept extension), e.g. a `storage:` extension of `tail_sampling`. Extensions used only by replaced receivers/exporters (auth, exporter queues) or by nothing (`health_check`, `pprof`, `zpages`, which would bind fixed ports) are removed. Each removal is reported. |
| `service.telemetry` | Replaced: `metrics.level: none` to avoid binding :8888 *(verified)*; logs at the user's level, to stderr, unsampled, so they can be shown on failure. Self-tracing is dropped. |
| added | `health_check/otel_test` on a free port, when the build has `health_check`, for readiness. |

The rewritten config is written to a fresh temporary directory (file mode
0600, since processor settings may contain literal credentials) and validated
with `otelcol validate` before the Collector is started; a validation failure is
a configuration error (exit code 3), never a test result. The Collector runs
with the **user config's directory as its working directory**, so relative
paths and `${file:./x}` resolve as they would when run next to the config.

Generated capture exporters use `encoding: proto`, `compression: none`,
`timeout: 5s`, `retry_on_failure.enabled: false`, `sending_queue.enabled:
false`. Without a queue the export runs synchronously inside the pipeline: when
no buffering processor (e.g. `batch`) sits in front of it, a failed capture
delivery is returned to the injecting client as an HTTP 5xx *(verified)*.

**Caveat:** the OTLP/HTTP exporter in v0.164.0 *silently ignores unknown
keys* *(verified: `encodng: proto` passes validation, while `batch` and `debug`
reject unknown keys)*, so `otelcol validate` can not vouch for these settings.
An integration test resolves a rewritten config with `otelcol print-config` and
asserts each setting took effect.

## Collector binary

Resolution order: `--collector-bin`, `OTEL_TEST_COLLECTOR_BIN`, the cached
pinned release, then a download (unless `--offline`). The pinned release's
SHA-256 for 12 platforms is compiled in. The archive is hashed while streaming
to disk and opened only after the hash matches; only a regular file named
`otelcol-contrib` at the archive root is extracted (links and nested paths are
ignored) and moved into `<cache>/otelcol-contrib/<version>-<platform>/` by
atomic rename, so an entry exists in the cache only if it was verified. The
cache defaults to the platform cache dir (`~/Library/Caches/otel-test`,
`$XDG_CACHE_HOME/otel-test`, `%LOCALAPPDATA%\otel-test`) and can be moved with
`OTEL_TEST_CACHE_DIR`; `OTEL_TEST_DOWNLOAD_BASE_URL` selects a mirror
(checksums still enforced). Cached binaries are not re-hashed on every run
(≈370 MB); the cache is trusted to the same degree as the user's home directory.

On startup the binary is probed with `--version` and `components`. otel-test
requires the `otlp` receiver and an OTLP/HTTP exporter (`otlp_http`, or
`otlphttp` on older builds); `health_check` is used when present.

## Collector process

Started with stdout/stderr captured into a bounded buffer (last 2000 lines).
It is ready when **every injection receiver port accepts TCP connections and
the generated health check returns HTTP 200 with "Server available"**. The
health check matters because ports are chosen by binding `127.0.0.1:0` and
releasing them: if another process grabbed a port in between, a bare TCP check
would be fooled, whereas the health check can only be served by this Collector,
and the Collector itself exits on the bind failure (reported as a startup
error with its log tail). Typical time to ready on an M-series Mac is
~150 ms warm, ~900 ms on first launch; the default startup timeout is 30 s.
On timeout the Collector is killed. Shutdown sends SIGTERM and kills after a
10 s grace period. On Windows the Collector is killed immediately: there is no
SIGTERM, and a console control event would need `unsafe` Win32 calls, which the
workspace forbids. Buffering processors therefore do not flush on Windows, and
Windows support is untested. A Collector that exits after becoming
ready is detected via `RunningCollector::exit_status`.

## Capture server

`otel-test-capture` binds two listeners on ephemeral `127.0.0.1` ports:

* **OTLP/HTTP** at `POST /<slug>/v1/{traces,metrics,logs}`. Bodies may be
  `application/x-protobuf` or `application/json` (standard OTLP/JSON),
  optionally `Content-Encoding: gzip`; the response is an empty
  `Export*ServiceResponse` in the request's encoding. Unknown exporter slug or
  signal → 404, unsupported media type or encoding → 415, undecodable body →
  400. GET → 405.
* **OTLP/gRPC** implementing `TraceService`, `MetricsService` and `LogsService`.
  Service paths are fixed by the protocol, so the exporter identity travels in
  the `x-otel-test-exporter` metadata header, set through the rewritten
  `otlp_grpc` exporter's `headers`. Missing header → `INVALID_ARGUMENT`,
  unknown slug → `NOT_FOUND`. gRPC compression is not enabled (rewritten
  exporters use `compression: none`).

The rewriter assigns each original exporter a path-safe *slug*
(`otlp_http/backend` → `otlp_http_backend`); `session::prepare` registers
slug → original ID with the store, and every capture is recorded under the
**original exporter ID**. Requests for unregistered slugs are rejected and
kept as diagnostics (`Store::take_rejected`), never counted as telemetry.
HTTP is the default capture protocol; gRPC is supported and exercised end to
end by the integration tests.

Requests are decoded into `opentelemetry-proto` messages and stored in memory
behind a mutex. Captured telemetry is partitioned by **windows**: at most one is
open at a time; everything received while it is open belongs to it, and
anything received while none is open is kept as *unattributed*
(`Store::take_unattributed`) so late or stray deliveries are visible but can
never leak into a test result. Dropping a window closes it, so a panic can not
leave the store wedged. `Window::changed()` wakes waiters on every new request,
so positive assertions can finish as soon as data arrives instead of sleeping.
Item counts are spans, log records or metric data points (one per histogram /
summary point).

## Test execution

`runner::plan` first resolves every selected test against the rewritten
configuration and reports all problems together (exit code 2): fixture
invalid or of the wrong signal, receiver ambiguous or not feeding that signal,
assertion naming an exporter no pipeline uses (or a connector).

Each test then runs in **its own Collector process**, started from the same
rewritten config and ports:

1. Start the Collector and wait for readiness.
2. Open a capture window, POST the fixture verbatim as OTLP/JSON to the chosen
   injection receiver, and require HTTP 2xx without an OTLP `partialSuccess`
   rejection. Anything else is an execution error (exit 5), never a result.
3. Observe for the test's `timeout` (default 3 s):
   * Tests containing `dropped`, `count` or `absent` observe the **full**
     timeout, since those outcomes can still turn from pass to fail while
     telemetry arrives.
   * Tests with only positive assertions finish early once every assertion
     holds and nothing new has arrived for 500 ms (`runner::SETTLE`).
   * If the Collector exits during the window, the test errors.
4. Stop the Collector gracefully (SIGTERM) **with the window still open**, then
   close the window. A non-zero exit is an error. Capture-side rejections
   (unknown exporter slug, undecodable request) are errors.
5. Evaluate every assertion on everything captured in the window.

### Why a fresh Collector per test

A shared Collector cannot isolate tests: a buffering processor releases a test's
telemetry after its window has closed, so it lands in the next test's window.
It would also make a `dropped` assertion pass for data that was only delayed,
and carry processor state (e.g. `tail_sampling` buffers) across tests.
Restarting costs ≈150–200 ms per test on an M-series Mac.

Graceful shutdown with the window open captures buffered output:
*(verified on v0.164.0 with a 30 s delay and a 1 s window)*

| Processor | Before shutdown | After graceful shutdown |
|-----------|-----------------|-------------------------|
| `batch` (`timeout: 30s`) | nothing | flushed, captured |
| `tail_sampling` (`decision_wait: 30s`, always_sample) | nothing | flushed, captured |
| `groupbytrace` (`wait_duration: 30s`) | nothing | **discarded** |
| `interval` (default 1 m) | nothing | flushed, captured |
| `span_metrics` connector (default `metrics_flush_interval: 1m0s`) | nothing | **nothing emitted** |
| `service_graph` connector (`metrics_flush_interval` unset, prints `null`) | nothing in 2 s | **nothing emitted** |

*(`interval`, `span_metrics` and `service_graph` verified on v0.164.0 with a 2 s
window; `span_metrics` and `service_graph` emit as expected with
`metrics_flush_interval: 300ms`.)*

Telemetry that arrives during shutdown is counted for the test and reported
as a verbose-mode note. Because `groupbytrace` discards pending traces, the
planner warns when a `groupbytrace` processor reachable from the test's
receiver (following connectors) has a `wait_duration` (default 1 s) not shorter
than the test's timeout; in that case `dropped` would pass for the wrong reason.
It warns likewise for `span_metrics`/`service_graph` connectors (and their
legacy names) whose `metrics_flush_interval` is unset or not shorter than the
timeout. With a short interval these connectors emit repeatedly (cumulative
temporality), so `count` on their output depends on timing; assert presence
and attributes instead.

### Assertion semantics

* Items are spans, log records or metric data points. Attribute assertions
  must hold for **every** captured item in scope (all exporters, or the one
  named by `exporter:`), and **fail if no item was captured**.
* `level: any` looks up item, then scope, then resource attributes and uses
  the first match; `absent` with `any` requires absence at all three levels.
* `equals` is type-strict (`500` ≠ `"500"`, `500` ≠ `500.0`); the failure
  message names the actual type. `matches` is an anchored regex and fails on
  non-string values.
* `dropped` passes only when the fixture was accepted, the Collector stayed up
  and exited cleanly, nothing was rejected by the capture server, and zero
  items reached any exporter over the full window plus shutdown.
* Failure messages list up to five offending items with their exporter, and
  count assertions show where telemetry went instead.
* Wording follows the signal each exporter actually received, falling back to
  the signals it is configured for: a trace fixture feeding `span_metrics`
  reports "data points" at the metrics exporter. `count` sums items of every
  signal an exporter received. Empty exports, such as an idle pipeline's flush
  at shutdown, are ignored.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | All selected tests passed |
| 1 | At least one assertion failed |
| 2 | Usage error or invalid test specification |
| 3 | Collector configuration could not be rewritten or failed `otelcol validate` |
| 4 | Collector binary unavailable or failed to start / became unready |
| 5 | Execution error (injection failed, Collector crashed mid-run, internal error) |
| 130 | Interrupted (Ctrl-C / SIGINT): the run is dropped, which kills the Collector (`kill_on_drop`) and removes temporary directories |

## Compatibility concerns (Collector Contrib v0.164.0)

1. **Exporter renames.** The OTLP exporters are now `otlp_grpc` and
   `otlp_http`; the legacy `otlp` and `otlphttp` names remain accepted as
   aliases *(verified)*. Classification must treat both spellings as the same
   type; generation must use a name the binary actually reports, so the
   rewriter queries `otelcol components` once and caches the result.
2. **Other underscore aliases.** Many components have dual names
   (`metricstransform`/`metrics_transform`, `k8sattributes`/`k8s_attributes`,
   …) *(verified via the "valid values" error list)*. Processors are copied
   verbatim, so this only matters if otel-test ever needs to reason about a
   processor's type (e.g. reading `batch` timeouts to warn about windows).
3. **Unused components are validated** *(verified)* — see the rewriting section.
4. **Self-telemetry port.** Default `service.telemetry.metrics` binds
   `localhost:8888`, which breaks concurrent runs. `level: none` disables it
   *(verified)*.
5. **Readiness.** The Collector logs `Everything is ready.` *(verified)* but log
   text is not a stable API. Readiness uses the `health_check` extension and
   receiver ports instead (see *Collector process*).
6. **Config providers.** `${env:X}` inside processor values is fine. A provider
   reference in a *structural* position (e.g. `exporters: ${file:x.yaml}`) can
   not be analysed without resolving; it is rejected with a clear error rather
   than guessed at.
7. **Time-based processors.** `batch` and `tail_sampling` flush on graceful
   shutdown and are handled by the per-test lifecycle; `groupbytrace` discards
   pending traces and triggers a planning warning (see *Test execution*).
8. **Rust dependencies.** `serde_yaml` 0.9.34 is marked deprecated upstream. It
   is used as specified; it is isolated behind `spec`/`config` so it can be
   swapped for a maintained fork (`serde_norway`, `serde_yaml_ng`) without API
   changes. `opentelemetry-proto` 0.33 depends on tonic/prost 0.14 and
   unconditionally on `opentelemetry`/`opentelemetry_sdk`; acceptable for an MVP.
9. **Release assets.** Each archive has a sibling `<archive>.sha256` file
   *(verified)*. The pinned version's checksums are compiled into otel-test, so a
   compromised release page can not supply its own checksum.
10. **Processors that need infrastructure.** `k8s_attributes` without
    `passthrough: true` logs `Could not initialize kube client` and never
    becomes ready outside a cluster *(verified; it was still not ready after 46 s)*.
    The startup error carries a hint, and error-level lines logged before the
    20-line tail are shown, because the cause is followed by a long stack trace.
    `passthrough: true` starts fine *(verified)*. `resource_detection` with
    cloud detectors (`ec2`) starts but adds its detector timeout to every
    test's startup (≈4 s with `timeout: 2s` off-cloud) *(verified)*.
