# Sampling and filtering

Cost control in front of a SaaS tracing backend. Kubernetes probe spans are
dropped first, then `tail_sampling` keeps only errors, slow requests (over 1 s)
and enterprise customers. Everything else is dropped.

Components: `filter` (OTTL, including `IsMatch` on the user agent),
`tail_sampling` (`status_code`, `latency` and `string_attribute` policies),
`memory_limiter`, `batch`.

```bash
otel-test test -f examples/real-world/03-sampling-filtering/otel-test.yaml
```

Each fixture is a **whole trace**. The error trace has three spans and only one
of them failed, so `count: 3` checks that tail sampling keeps the full trace and
not just the failing span.

## What is tested

| Test | Protects against |
|------|------------------|
| error traces are kept in full | losing errors, or keeping only partial traces |
| slow traces are kept | latency policy misconfigured |
| enterprise traces are kept | attribute policy misconfigured |
| fast healthy traces are not sampled | cost blow-up (e.g. a catch-all policy) |
| failing readiness probes are dropped before sampling | probe errors during rollouts being sampled as errors, which tests processor order |
| probes are identified by user agent | probes on unexpected routes |

## Regressions

| Patch | Change | Caught by |
|-------|--------|-----------|
| `errors-policy-removed` | `status_code` policy deleted | error trace count 3 |
| `latency-threshold-raised` | 1000 ms → 5000 ms | slow trace count 2 |
| `probe-user-agent-rule-removed` | `kube-probe` condition deleted | probe test `dropped` |
| `baseline-always-sample` | an `always_sample` policy added | fast trace `dropped` |

The last two show that `dropped: true` is not vacuous: when the config changes,
the same fixture is exported and the test fails.

## Timing

`decision_wait` is 500 ms and the test timeout is 2 s. `tail_sampling` also
decides pending traces at graceful shutdown (verified), so a longer
`decision_wait` still works. The `groupbytrace` processor does **not** flush
at shutdown; otel-test warns when its wait is not shorter than the timeout.

## Not covered

* Probabilistic policies: they hash the trace ID, so results are deterministic
  per fixture but say little about the configured rate.
* Rate limiting across many traces: one fixture is injected per test.
