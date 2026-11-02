# Changelog

## 0.1.0 (unreleased)

First release.

- `otel-test init`, `test`, `validate`, with `--filter`, `--verbose`, `--color`,
  `--collector-bin` and `--offline`.
- Test specification with attribute (`present`, `absent`, `equals`,
  `matches`, with `level` and `exporter` scoping), `dropped`, exporter-reached
  and exporter-count assertions; all spec problems reported at once with
  locations.
- Standard OTLP/JSON fixtures for traces, logs and metrics, injected verbatim.
- Collector config rewriting that replaces only receivers and exporters,
  preserving processors, connectors, processor order and exporter identities,
  validated with `otelcol validate`.
- Pinned otelcol-contrib v0.164.0 download with compiled-in SHA-256
  verification and caching.
- OTLP/HTTP (protobuf, JSON, gzip) and OTLP/gRPC capture server.
- A fresh Collector per test, with graceful shutdown inside the observation
  window so `batch`/`tail_sampling` output is attributed to the right test.
- Five real-world examples (`examples/real-world`) with deliberate
  regressions and `verify.sh`, which checks that each one is caught.
- Assertion messages use each exporter's own signal (e.g. "data points" at a
  metrics exporter fed by `span_metrics`), and empty exports are ignored.
- Warnings for `span_metrics`/`service_graph` connectors whose flush interval
  is not shorter than the test timeout.
- Startup failures show error-level Collector lines logged before the tail,
  plus a hint for `k8s_attributes` outside Kubernetes.
- Distinct exit codes for assertion failures, spec, config, startup and
  execution errors, and interruption.
