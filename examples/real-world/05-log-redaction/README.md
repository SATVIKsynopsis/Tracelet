# Log redaction and transformation

Application logs go to Loki and to a compliance archive. Some services log
**JSON strings as the body**, so secrets can hide inside the body as well as in
attributes. One `transform` processor (OTTL):

1. parses JSON bodies into attributes (`ParseJSON` + `merge_maps`) and promotes
   `message` to the body;
2. deletes every attribute whose key looks like a secret
   (`delete_matching_keys`);
3. pseudonymizes `user.id` with `SHA256`, so logs stay joinable;
4. masks the local part of e-mail addresses and truncates client IPs to /24.

`filter` drops DEBUG logs. Receivers: `otlp` and `filelog` (with a
`json_parser` operator). Exporters: `otlp_http` (Loki) and `file`.

```bash
otel-test test -f examples/real-world/05-log-redaction/otel-test.yaml
```

## What is tested

| Test | Protects against |
|------|------------------|
| secrets inside JSON bodies are removed | secrets surviving because they were in the body |
| secret attributes are removed by key pattern | `db.password`, `reset.token`, `x-api-key` leaking |
| user identifiers are pseudonymized | raw user IDs and e-mails stored; the exact SHA-256 is checked |
| JSON-body logs are pseudonymized too | the same rules not applied to parsed fields; both exporters receive the record |
| debug logs are dropped | noise and cost |
| file logs get the same treatment | the `filelog` pipeline differing from OTLP's |

## Regressions

| Patch | Change | Caught by |
|-------|--------|-----------|
| `delete-before-json-parse` | secret deletion moved before JSON parsing | `password`/`session_token` absent (JSON and file tests) |
| `token-pattern-dropped` | `token` removed from the key pattern | `session_token`, `reset.token` |
| `hash-removed` | `SHA256` statement deleted | exact hash and hex pattern |
| `debug-filter-not-in-pipeline` | processor defined but not listed in the pipeline | `dropped` |

## Found while writing this example

The first version used the pattern `api[_.]?key`, which does **not** match
`x-api-key` (hyphen). The structured-log test failed, showing the live key
`sk_live_51Hx` at both exporters. The pattern is now `api[-_.]?key`.

## Not covered

* **Log bodies, severity and trace context are not assertable.** The tests
  check the parsed fields as attributes, but not that the body became
  `"login succeeded"`.
* The `filelog` receiver and its `json_parser` operator do not run.
  otel-test replaces receivers, so injecting through `filelog/app` tests only
  the processors after it. That is why this config parses JSON bodies in
  `transform`, where it can be tested.
