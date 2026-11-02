# Metrics transformation

A metrics gateway in front of Mimir:

1. renames the legacy `http.status_code` label to `http.response.status_code`
   (`transform`, datapoint context);
2. removes the high-cardinality `user.id` label by **summing it away**
   (`metrics_transform`, `aggregate_labels`);
3. drops Go runtime and `debug.*` metrics (`filter`);
4. sets `deployment.environment.name` and deletes `host.ip` (`resource`).

The order matters: aggregation keeps only `http.route` and
`http.response.status_code`, so the rename must run first.

Receivers: `otlp` and `prometheus` (scrape config). Exporters:
`prometheus_remote_write` with `basicauth`, and the pull-based `prometheus`
exporter.

```bash
otel-test test -f examples/real-world/04-metrics-transform/otel-test.yaml
```

## What is tested

The request fixture has 4 data points for 3 distinct (route, status) series.
After aggregation, exactly 3 data points must reach **each** exporter.

| Test | Protects against |
|------|------------------|
| user.id is aggregated away | cardinality explosion; aggregation not happening |
| legacy status code is renamed | broken dashboards |
| environment is stamped and host.ip removed | wrong environment label; leaking internal IPs |
| runtime metrics are dropped | paying for noise |
| scraped metrics get the same treatment | the `prometheus` receiver's pipeline differing from OTLP's (injected through the `prometheus` receiver's slot) |

## Regressions

| Patch | Change | Caught by |
|-------|--------|-----------|
| `processor-order-swapped` | aggregation before the rename | count 3 becomes 2; `http.response.status_code` missing |
| `user-id-kept` | `user.id` added to `label_set` | `user.id is absent`, count 3 becomes 4 |
| `filter-regex-typo` | `^go_.*` → `^go-.*` | runtime `dropped`; count in the mixed test |
| `resource-insert-not-upsert` | `upsert` → `insert`, so an existing `staging` value is never overwritten | `deployment.environment.name equals "production"` |

## Limitations shown by this example

* **Metric names and values are not assertable.** otel-test checks attributes
  and data-point counts. It cannot verify that `aggregate_labels` *summed*
  (40 + 25 = 65) rather than averaged, or that a metric was renamed.
  `count` and attribute assertions catch most mistakes, but not these.
* `equals` must hold for every data point, so a label with different values
  per point (here 200 and 500) can only be checked with `present`. `matches`
  works on strings only.
* The `prometheus` receiver's scraping and relabeling do not run. Its pipeline
  is tested by injecting OTLP into its slot.
* `cumulative_to_delta` and other stateful processors need a series of points
  over time. One fixture per test cannot exercise them meaningfully.
