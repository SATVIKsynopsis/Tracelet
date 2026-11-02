# PII redaction with the `redaction` processor

A payment service's spans carry card numbers, e-mail addresses, an
`Authorization` header and a password. The gateway Collector applies an
**allowlist**: every attribute that isn't listed is deleted, and values that
look like card numbers or e-mail addresses are masked even on allowed keys.

Components: `redaction`, `memory_limiter`, `batch`, an `otlp_http` exporter with
a file-backed sending queue (`file_storage` extension).

```bash
otel-test test -f examples/real-world/01-pii-redaction/otel-test.yaml
```

## What is tested

| Test | Protects against |
|------|------------------|
| attributes outside the allowlist are deleted | card number, password and auth header leaving the Collector |
| e-mail addresses are masked on allowed keys | `blocked_values` patterns being lost |
| operational attributes survive redaction | **over-redaction**: an allowlist so strict that `http.route`, the status code, the amount (a double) or `service.name` disappear |

## Regressions

`regressions/*.patch` are realistic edits; `../verify.sh` applies each one and
checks that the listed assertion fails.

| Patch | Change | Caught by |
|-------|--------|-----------|
| `allowlist-password` | `user.password` added to `allowed_keys` "for debugging" | `user.password is absent` |
| `email-mask-removed` | e-mail regex dropped from `blocked_values` | `user.email equals "****"` |

## Found while writing this example

The first version of this config forgot that **the allowlist also applies to
resource attributes**. Every span lost `service.name`. The over-redaction test
caught it on the first run; the config now lists `service.name` and
`deployment.environment.name`.

## Not covered

* The `redaction` processor also adds summary attributes
  (`redaction.redacted.keys`, `redaction.masked.count`, …). They are not
  asserted here.
* The exporter's `sending_queue` and `file_storage` extension are replaced by
  otel-test, so persistence is not exercised.
