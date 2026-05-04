# Performance Budgets

Four hot-path budgets from PLAN.md with measured baselines.

## Budgets & Measured Numbers

| Bench                                                        | Budget  | Measured (mean) | Verdict |
| ------------------------------------------------------------ | ------- | --------------- | ------- |
| Cold start proxy (`from_pool` + migrate + `list_folders`)    | <200 ms | ~0.6 ms         | ✓ PASS  |
| Folder-switch (`list_messages` 200-row limit, 100k messages) | <50 ms  | ~0.9 ms         | ✓ PASS  |
| Local search FTS5 (`search "hello"`, limit 50, 100k indexed) | <100 ms | ~39 ms          | ✓ PASS  |
| JWZ threader ingest — 100 msgs (baseline)                    | —       | ~60 ms          | —       |
| JWZ threader ingest — 1000 msgs (scaling)                    | —       | ~484 ms         | —       |

Numbers captured on 2026-05-03, Linux 6.19 / release build, in-memory SQLite.

## How to Run

```bash
rtk cargo bench -p inbx-store
```

Criterion HTML reports land in `target/criterion/`.

## Caveats

- **In-memory SQLite**: all benches use `:memory:`. Real-disk numbers are higher
  depending on fsync policy and cold-page cache. These are lower bounds.
- **Cold start budget** (PLAN: <200 ms): The bench measures only the
  `Store::from_pool` + migration path, not binary startup time. Binary startup
  (linking, dynamic loader, tokio init) adds ~10–50 ms depending on the host;
  this is not measurable with criterion.
- **Threader scaling**: ingest_1000 at ~484 ms is proportional to the O(n²)
  per-message cycle-check walk. With real mail (mostly shallow threads) the
  constant factor is much lower; the bench uses a worst-case mix.
- **Memory budget** (PLAN: <200 MB for 100k indexed): NOT benchmarkable with
  criterion. Use `valgrind --tool=massif` or `heaptrack` for manual profiling:

  ```bash
  heaptrack target/release/inbx-sync
  # or
  valgrind --tool=massif --pages-as-heap=yes target/release/inbx
  ```
