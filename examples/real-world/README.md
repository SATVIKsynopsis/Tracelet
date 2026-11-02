# Real-world examples

Five independent, production-style Collector configurations, each with an
otel-test suite and a set of **deliberate regressions** that the suite must
catch. They run against the pinned otelcol-contrib v0.164.0, with no mocks.

| Example | Scenario | Key components | Tests | Regressions |
|---------|----------|----------------|------:|------------:|
| [01-pii-redaction](01-pii-redaction) | allowlist and masking of span attributes | `redaction` | 3 | 2 |
| [02-tenant-routing](02-tenant-routing) | per-tenant backends, default route, error fan-out | `routing`, `forward`, `filter` | 5 | 3 |
| [03-sampling-filtering](03-sampling-filtering) | drop probes, keep errors, slow and enterprise traces | `filter`, `tail_sampling` | 6 | 4 |
| [04-metrics-transform](04-metrics-transform) | semconv rename, cardinality reduction, runtime noise | `transform`, `metrics_transform`, `filter`, `resource` | 5 | 4 |
| [05-log-redaction](05-log-redaction) | secrets in JSON bodies, pseudonymization, IP truncation | `transform` (OTTL), `filter` | 6 | 4 |

Run one suite:

```bash
otel-test test -f examples/real-world/02-tenant-routing/otel-test.yaml
```

The Collector configs reference credentials (`${env:VENDOR_API_KEY}`, …) and
production endpoints exactly as a real config would. otel-test replaces the
exporters, so the variables need not be set.

## Positive and negative validation

```bash
examples/real-world/verify.sh                    # every example, about 7 minutes
examples/real-world/verify.sh 03-sampling-filtering
```

For each example, `verify.sh`:

1. runs the suite against the original `collector.yaml`, which must exit 0;
2. for every `regressions/<name>.patch`:
   * applies the patch;
   * runs the suite, which must exit 1, and every
     `test name :: assertion` line in `regressions/<name>.expect` must be
     reported as failed;
   * reverts the patch, checks that `collector.yaml` is byte-identical to the
     original, and runs again, which must exit 0.

It needs `patch` and `shasum`. It uses `otel-test` from `PATH`, or
`target/release/otel-test`, or `$OTEL_TEST`.

## Bugs these examples found in the configs themselves

Both were mistakes made while writing these examples. They are kept in the
READMEs because they are the kind of mistake otel-test exists to catch.

* **01:** a `redaction` allowlist also strips *resource* attributes, so every
  span lost `service.name`.
* **05:** the secret-key pattern `api[_.]?key` missed `x-api-key`, so a live
  API key reached Loki.
