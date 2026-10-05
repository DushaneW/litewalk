# litewalk

`litewalk` is a dependency-free Rust implementation of the SQLite on-disk format: a library and a
command-line tool that read database files directly, without SQLite, without executing SQL, and
without ever writing.

It exists to make the file format inspectable: what is on page 42, which object owns it, whether
the file is structurally sound, what a WAL contains, how much unused space a leaf page holds.
It is not a database engine and will not become one.

```console
$ litewalk check app.db
Checking app.db

✓ SQLite header
✓ page boundaries
✓ page types
✓ cell pointers
✓ B-tree structure
✓ B-tree ordering
✓ overflow chains
✓ freelist
✓ schema
✓ record payloads
✓ page usage

Database is structurally valid.
```

## What makes it different

* **Verified against SQLite itself.** The test suite generates databases with SQLite (bundled
  `rusqlite`, a dev-dependency only) and compares every result bit for bit: scans, lookups,
  index entries, defaults, encodings, WAL overlays, header fields.
* **Treats the file as hostile.** Corruption becomes a structured error with location context,
  never a panic. 36 deterministic corruptions each assert *which* check catches them; 6,000
  mutated databases and 4,000 random inputs run in every `cargo test`; libFuzzer targets are
  included.
* **Small and readable.** No production dependencies, no `unsafe` (`forbid`), about 7,400 lines
  including the CLI. The architecture is described in [docs/](docs/architecture.md).

## Install and use

```console
$ cargo install --path .          # or: cargo build --release
$ litewalk info app.db
$ litewalk check app.db --json
$ litewalk page app.db 42 --hex
$ litewalk map app.db
```

Requires Rust 1.75 or newer.

### Commands

| command | output |
|---|---|
| `info <db>` | header fields, journal mode, table and index counts |
| `tables`, `indexes`, `objects`, `schema <db>` | schema objects, columns, types, defaults, root pages |
| `page <db> <n> [--hex]` | decoded page: type, cells, free space, overflow references; hex dump annotated with regions |
| `map <db>` | which table, index, overflow chain or freelist owns each page |
| `check <db>` | structural validation; exit status 1 if invalid |
| `stats <db> [--cache N]` | page cache hits, misses and hit ratio for a full read |
| `wal <db>` | frames, commit boundaries, salts, checksum status of `<db>-wal` |
| `forensic <db> [--free-space]` | structural report; with `--free-space`, unused regions inside b-tree pages (experimental) |
| `dump <db> <table>` | rows as TSV, CSV or JSON: `--format --limit --offset --columns --rowid --pretty --hex-blobs` |
| `get <db> <table> <rowid>` | one row by rowid |

Common options: `--json` (every command except `dump`, which has `--format json`), `--verbose`,
`--limit`, `--offset`, `--with-wal` (apply committed frames from `<db>-wal`), `--cache N`.
Exit status: 0 success, 1 failure (including an invalid database from `check`), 2 usage error.
Data goes to stdout, diagnostics to stderr.

### Library

```rust
use litewalk::Database;

let mut db = Database::open("app.db")?;
for table in db.tables()? {
    println!("{} {:?}", table.name, table.column_names());
}
let users = db.table("users")?;
for row in db.scan(&users)? {          // rowid order; stops after the first error
    let row = row?;
    println!("{:?} {:?}", row.rowid, row.values);
}
let one = db.get(&users, 4711)?;       // binary search down the b-tree
let report = db.check()?;              // structural validation
assert!(report.is_valid());
```

The handle takes `&mut self` because reading seeks the source and updates the page cache.
`Database::from_bytes`, `from_reader`, `open_with_wal` and `with_options` cover other sources.
`Database::for_each_record` hands out borrowed `ValueRef`s without per-value allocation. API
documentation: `cargo doc --open`.

## Supported and unsupported

| Feature | Status |
|---|---|
| SQLite header | Supported |
| UTF-8 | Supported |
| UTF-16le / UTF-16be | Supported, tested against SQLite |
| Table b-trees | Supported |
| Index b-trees | Supported (structure and scans; key order is not verified) |
| Overflow pages | Supported |
| `WITHOUT ROWID` | Supported for scans (no key lookup) |
| Generated columns | STORED supported; VIRTUAL reported unsupported (needs an SQL engine) |
| `ALTER TABLE ADD COLUMN` defaults | Literals only; expressions reported unsupported |
| Freelist | Supported (walked and checked) |
| Auto-vacuum | Read and checked; pointer-map entries not decoded |
| WAL | Read-only: inspection and committed-frame overlay |
| Rollback journal, encrypted databases, virtual tables | Not supported |
| Integrity checking | Supported; not a replacement for `PRAGMA integrity_check` |
| Writing, repair, SQL | Not supported, by design |

Details and caveats: [docs/compatibility.md](docs/compatibility.md).

## Security model

Database files are untrusted input. litewalk never executes SQL, never modifies its input, never
follows a page number it has not range-checked, bounds every traversal and size, and reports
violations as `Error::Corrupt` with location context. A panic, hang or unbounded allocation on
any input is a bug; see [SECURITY.md](SECURITY.md) for scope and reporting.

## Testing

`cargo test` runs 70 tests (several of them loop over page sizes, corruption cases or thousands of generated inputs):

| area | what is checked |
|---|---|
| differential (`tests/differential.rs`, `formats.rs`) | scans, rowid lookups, index entries, WITHOUT ROWID, UTF-16, generated columns, defaults, rowid-alias rules, header fields, against SQLite, at page sizes 512 to 65536 |
| integrity (`tests/check.rs`) | SQLite-written databases (all page sizes, deletes, updates, freelists, auto-vacuum, vacuumed, views, triggers) produce zero issues; 36 corruptions are each caught by the expected check |
| WAL (`tests/wal.rs`) | frames, checksums and commit markers of WAL files SQLite wrote; overlay equals SQLite's view; uncommitted frames excluded; damaged logs |
| CLI (`tests/cli.rs`) | the real binary: every `--json` output parses as JSON, formats, exit statuses |
| invariants (`tests/fuzz_invariants.rs`) | 6,000 structurally mutated databases, 4,000 random inputs, every two-byte record header prefix: no panic, no hang |

Bugs found while building this are recorded in the changelog: the differential tests found a
wrong REAL-affinity conversion; code review found an unbounded allocation from a corrupt payload
size.

### Fuzzing

`fuzz/` contains cargo-fuzz targets `fuzz_header`, `fuzz_varint`, `fuzz_record`, `fuzz_wal` and
`fuzz_database`. They call the same invariant functions as `tests/fuzz_invariants.rs`: arbitrary
input yields a value or a controlled error, never a panic, hang or runaway allocation.

```console
$ cargo install cargo-fuzz        # needs a nightly toolchain
$ cargo +nightly fuzz run fuzz_database -- -max_total_time=600
```

**Status: the targets compile (checked with `cargo check`), but libFuzzer has not been run on
them.** The environment this was developed in had only a stable toolchain. The invariants have
been exercised only by the seeded mutation harness above. That is weaker than coverage-guided
fuzzing, and no claim is made that fuzzing finds nothing.

## Benchmarks

`cargo run --release --example bench`. One session on a single-core Xeon 2.8 GHz VM; SQLite is the
bundled 3.45 through `rusqlite`. These are indicative numbers, not a comparison of engines:
SQLite's side executes a prepared statement, litewalk's walks a b-tree. Methodology and caveats:
[docs/benchmarking.md](docs/benchmarking.md).

#### Full scans (payload 100 B, page size 4096, cache 256 pages)

| rows | file | litewalk `scan` | litewalk `for_each_record` | SQLite `SELECT *` |
|---:|---:|---:|---:|---:|
| 10000 | 1.1 MiB | 5.5 ms | 3.8 ms | 3.3 ms |
| 100000 | 11.5 MiB | 49.9 ms | 31.4 ms | 29.5 ms |
| 1000000 | 115.1 MiB | 511.3 ms | 332.3 ms | 257.5 ms |

#### Lookups by rowid (100000 rows, payload 100 B, page size 4096)

| lookups | litewalk, no cache | litewalk, 256 pages | SQLite |
|---:|---:|---:|---:|
| 1 | 11 µs total | 8 µs total | 17 µs total |
| 100 | 733 µs total | 624 µs total | 1415 µs total |
| 1000 | 4.9 ms (4.9 µs each) | 4.4 ms (4.4 µs each) | 8.5 ms (8.5 µs each) |
| 100000 | 538.1 ms (5.4 µs each) | 380.3 ms (3.8 µs each) | 842.8 ms (8.4 µs each) |

#### Payload size (about 32 MiB of text per table, page size 4096, cache 256 pages)

| payload | rows | file | litewalk `scan` | litewalk `for_each_record` | SQLite `SELECT *` |
|---:|---:|---:|---:|---:|---:|
| 10 B | 200000 | 5.3 MiB | 83.4 ms | 45.7 ms | 38.6 ms |
| 1 KB | 32000 | 31.3 MiB | 34.2 ms | 26.1 ms | 20.9 ms |
| 10 KB | 3200 | 31.3 MiB | 23.0 ms | 20.4 ms | 14.6 ms |
| 100 KB | 320 | 30.6 MiB | 22.8 ms | 22.3 ms | 12.6 ms |
| 1 MB | 32 | 30.6 MiB | 26.7 ms | 19.7 ms | 38.1 ms |

#### Page size (100,000 rows, payload 100 B, cache 256 pages)

| page size | file | pages | litewalk `scan` | SQLite `SELECT *` | 10,000 lookups litewalk | 10,000 lookups SQLite |
|---:|---:|---:|---:|---:|---:|---:|
| 512 | 12.5 MiB | 25503 | 79.9 ms | 44.7 ms | 40.0 ms | 88.4 ms |
| 1024 | 12.3 MiB | 12628 | 67.5 ms | 34.4 ms | 38.8 ms | 71.9 ms |
| 2048 | 11.6 MiB | 5914 | 56.1 ms | 35.8 ms | 36.5 ms | 73.0 ms |
| 4096 | 11.5 MiB | 2937 | 52.0 ms | 25.7 ms | 36.0 ms | 75.7 ms |
| 8192 | 11.4 MiB | 1460 | 50.5 ms | 25.2 ms | 50.2 ms | 77.5 ms |
| 16384 | 11.3 MiB | 725 | 48.9 ms | 24.6 ms | 64.7 ms | 93.4 ms |
| 32768 | 11.3 MiB | 362 | 49.7 ms | 22.6 ms | 64.8 ms | 107.4 ms |
| 65536 | 11.4 MiB | 182 | 51.9 ms | 21.6 ms | 33.5 ms | 142.4 ms |

#### Page cache capacity (100,000 rows, 2937 pages of 4096 B, 100,000 random lookups)

| cache pages | lookups | hit ratio | full scan |
|---:|---:|---:|---:|
| 0 | 533.6 ms | 0.0% | 52.5 ms |
| 64 | 340.0 ms | 67.3% | 48.1 ms |
| 256 | 356.7 ms | 69.5% | 48.3 ms |
| 1024 | 352.5 ms | 78.3% | 50.6 ms |
| 4096 | 276.7 ms | 99.0% | 50.3 ms |

What they show: litewalk's decoded-row scan is 1.7 to 2 times slower than `SELECT *` on small
rows; the borrowed `for_each_record` is close to SQLite there but not for mid-size payloads;
point lookups are 4 to 8 microseconds; the cache cuts random-lookup time by roughly a third to a
half and does nothing for scans. The first measurement of lookups (18 microseconds, slower than SQLite
even with a 99% cache hit rate) led to rewriting `get` to decode O(log n) cells per page.

## Development

```console
$ cargo fmt --check
$ cargo clippy --all-targets --all-features -- -D warnings
$ cargo test
$ cargo doc --no-deps
```

Layout: `src/` library and `src/cli/`; `tests/` integration tests with a SQLite-backed fixture
module in `tests/common/`; `fuzz/` libFuzzer targets; `examples/bench.rs`; `docs/`.

## Further reading

* SQLite file format: <https://www.sqlite.org/fileformat.html>
* WAL format: <https://www.sqlite.org/walformat.html>
* Record format: <https://www.sqlite.org/fileformat2.html#record_format>
* Datatypes and affinity: <https://www.sqlite.org/datatype3.html>

## License

MIT
