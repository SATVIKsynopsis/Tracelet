# Fixtures

Fixtures are **standard OTLP/JSON** documents — exactly the body an OTLP/HTTP
client sends with `Content-Type: application/json` to `/v1/traces`,
`/v1/logs` or `/v1/metrics`. There is no otel-test-specific format and no
conversion: what you write is what the Collector's OTLP receiver decodes.

| Top-level key     | Signal  | Protobuf message               |
|-------------------|---------|--------------------------------|
| `resourceSpans`   | traces  | `ExportTraceServiceRequest`    |
| `resourceLogs`    | logs    | `ExportLogsServiceRequest`     |
| `resourceMetrics` | metrics | `ExportMetricsServiceRequest`  |

The signal is inferred from the top-level key unless the test sets `signal:`.

OTLP/JSON rules that commonly trip people up
([spec](https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding)):

* `traceId` / `spanId` are **hex** strings (32 / 16 characters), not base64.
* 64-bit integers are **strings**: `"intValue": "500"`, `"timeUnixNano": "1735689600000000000"`.
* Enums are integers: span `kind` (`2` = SERVER), status `code` (`1` = OK, `2` = ERROR),
  metric `aggregationTemporality` (`2` = CUMULATIVE).
* Field names are lowerCamelCase.

All fixtures here use fixed IDs and timestamps so test runs are deterministic.
Each one was checked by POSTing it to `otelcol-contrib` v0.164.0 with a
`debug` exporter.

| File | Contents |
|------|----------|
| `traces/app-span.json` | Normal `GET /api/orders` server span, status OK |
| `traces/healthz-span.json` | `GET /healthz` span (`http.route = /healthz`) |
| `traces/error-span.json` | `POST /api/charge` on `payment-api`, status ERROR, `http.response.status_code = 500` |
| `traces/login-span.json` | `POST /login` span with `password` and `user.name` attributes |
| `logs/login-log.json` | Log record with `password`, `user.name`, `client.address` attributes |
| `metrics/http-requests.json` | Cumulative monotonic sum with two data points (status 200 and 500) |
