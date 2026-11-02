# Multi-tenant routing

A shared gateway receives spans from many tenants. Each known tenant's spans
must reach **only** that tenant's backend. Spans from unknown tenants, or with
no tenant at all, go to a shared Kafka topic. Error spans from any tenant are
also copied to the on-call alerting backend.

Components: the `routing` connector (resource context, `default_pipelines`),
the `forward` connector for fan-out, and `filter` on the alerting branch.
Exporters: `otlp_http`, `otlp_grpc` with a `bearertokenauth` extension, and
`kafka`.

```bash
otel-test test -f examples/real-world/02-tenant-routing/otel-test.yaml
```

## What is tested

| Test | Protects against |
|------|------------------|
| acme spans reach only the acme backend | cross-tenant leaks; normal traffic paging on-call |
| globex spans reach only the globex backend | cross-tenant leaks |
| unknown tenants go to the shared topic | data loss for new tenants |
| spans without a tenant go to the shared topic | data loss when the attribute is missing |
| errors reach the tenant backend and alerting | alerts silently lost |

`count: 0` assertions wait for the full timeout, so "never reaches" is
observed rather than assumed.

## Regressions

| Patch | Change | Caught by |
|-------|--------|-----------|
| `globex-routed-to-acme` | copy-paste error in the routing table | globex count 1, acme count 0 |
| `no-default-route` | `default_pipelines` removed | both shared-topic tests |
| `alert-filter-inverted` | `!=` changed to `==` in the alerting filter | alerting counts in two tests |

## Not covered

* The Kafka exporter's topic and encoding, and the gRPC exporter's auth, are
  replaced by otel-test. What is tested is *which exporter* receives the data.
* Routing on request metadata (`context: request`, e.g. an HTTP header) is
  **not testable**: fixtures are injected by otel-test without custom headers.
