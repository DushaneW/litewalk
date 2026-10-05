# Benchmarking

```
cargo run --release --example bench -- [scan|lookup|payload|pagesize|cache|all]
```

The harness prints Markdown tables of medians (5 runs; 3 for the slowest cases) with SQLite via
`rusqlite` as a reference point. It is intentionally simple: no outlier rejection, no confidence
intervals, a warm OS file cache, and a prepared `SELECT` as SQLite's side. Criterion was not used
because it adds a large dependency tree and the numbers are only meaningful for the machine they
were taken on. Compare rows within one table, not across machines.

## What each sweep varies

| sweep | varies | fixed |
|---|---|---|
| scan | 10k, 100k, 1M rows | 100 B payload, 4096 pages, 256-page cache |
| lookup | 1, 100, 1k, 100k random rowids | 100k rows, 4096 pages |
| payload | 10 B, 1 KB, 10 KB, 100 KB, 1 MB | about 32 MiB per table |
| pagesize | 512 ... 65536 | 100k rows |
| cache | 0, 64, 256, 1024, 4096 pages | 100k rows, 100k random lookups |

## Results from the development machine

One session on a single-core Xeon 2.8 GHz VM (Rust 1.75, release build, SQLite 3.45 bundled).
These are the numbers the author measured; re-run them before relying on any of them.

Full scan, 100,000 rows (11.5 MiB): `scan` 50 ms, `for_each_record` 31 ms, SQLite `SELECT *` 30 ms.
At 1,000,000 rows: 511 / 332 / 258 ms.

Random rowid lookups, 100,000 of them: 538 ms without a cache, 380 ms with 256 pages, SQLite
843 ms (5.4, 3.8 and 8.4 microseconds each). SQLite's figure includes executing a prepared
statement through `rusqlite`, which does more than a b-tree descent, so this is not a statement
about SQLite's b-tree code.

Cache effect (100,000 random lookups over 2,937 pages): hit ratio 0% / 67% / 70% / 78% / 99% for
0 / 64 / 256 / 1024 / 4096 cached pages; time 534 / 340 / 357 / 353 / 277 ms. The cache helps
random lookups modestly and full scans not at all (each page is read once).

The full tables are in the README.

## What the benchmarks changed

The first run showed lookups taking 18 microseconds each even at a 99% cache hit rate, 2.2 times
slower than SQLite, which pointed at CPU work rather than I/O: `get` decoded every cell of every
page on its path. It now binary-searches the page and decodes O(log n) cells per page, which made
lookups 3.4 to 4.4 times faster (18.3 to 5.4 microseconds uncached, 16.6 to 3.8 with the cache).
The scan path is still 1.7 to 2 times slower than SQLite for decoded rows on 100-byte payloads.
`for_each_record`, which skips allocating owned values, closes most of that gap on small rows
(6-29% slower) but not on mid-size payloads (SQLite 1.4 times faster at 10 KB and 1.8 times at
100 KB); at 1 MB litewalk was faster (20 against 38 ms) in this run, which I would not read much
into with 32 rows.

## Claims this does not support

* "Faster than SQLite" in general. The two sides do different work.
* Anything about databases that do not fit in the OS page cache.
* Anything about other hardware, other Rust versions, or concurrent access.
