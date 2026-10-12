<div align="center">

# litewalk

**Zero-Dependency, Memory-Safe SQLite On-Disk Format Parser, Forensic Inspector & B-Tree Reader in Rust**

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](#installation--platform-setup)
[![Unsafe](https://img.shields.io/badge/unsafe-forbid-success.svg)](#security--hostile-input-model)
[![Dependencies](https://img.shields.io/badge/dependencies-zero-blue.svg)](#key-engineering-highlights)
[![License: MIT](https://img.shields.io/badge/License-MIT-white.svg)](#license)
[![Platform](https://img.shields.io/badge/platform-Linux_%7C_macOS_%7C_Windows-lightgrey.svg)](#installation--platform-setup)

[Overview](#overview) · [Installation](#installation--platform-setup) · [CLI Reference](#cli-reference) · [Rust Library API](#rust-library-api) · [Format Support](#format-compatibility-matrix) · [Benchmarks](#performance--benchmarks)

</div>

---

## Overview

**litewalk** is a standalone, zero-dependency Rust library and command-line toolkit implementing the **SQLite 3 on-disk binary format** from scratch. It reads `.db` and `.db-wal` files directly from disk or memory—without linking `libsqlite3`, without executing SQL, and with strict read-only guarantees.

Built for database forensics, low-level storage inspection, and high-speed embedded data extraction, **litewalk** exposes the internal mechanics of SQLite files: inspecting what resides on page `42`, mapping page ownership across tables and indexes, verifying structural B-tree invariants, inspecting Write-Ahead Log (WAL) frames, and scanning unallocated free space inside leaf pages.

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

---

## Key Engineering Highlights

- **Zero Production Dependencies & `#![forbid(unsafe_code)]`:** Implemented in ~7,400 lines of pure, safe Rust (including the CLI). No C bindings or external runtime crates are linked in production builds.
- **Bit-for-Bit Differential Verification:** Continuously validated against canonical SQLite (`3.45+`) across page sizes from `512` to `65,536` bytes. Table scans, binary-search `rowid` lookups, index entries, default values, text encodings (`UTF-8`, `UTF-16LE`, `UTF-16BE`), and WAL overlays match reference outputs bit for bit.
- **Hostile-Input Hardened:** Treats every database file as untrusted input. Corrupted pointers, cyclic overflow chains, and malformed varints resolve deterministically to structured `Error::Corrupt` diagnostics with exact byte/page offsets—never panics or unbounded allocations.
- **Sub-10µs Point Lookups & Zero-Copy Scans:** Uses $O(\log n)$ binary cell search per B-tree page and provides a zero-allocation `for_each_record` visitor API (`ValueRef`) alongside an ergonomic iterator interface.

```text
+-------------------------------------------------------------------------+
|                    SQLite Database (.db)  +  WAL (.db-wal)              |
+------------------------------------+------------------------------------+
                                     |
                                     v
+-------------------------------------------------------------------------+
|                      I/O & Page Cache Layer (LRU)                       |
|      [ Header Parser ]  <--->  [ Page Cache ]  <--->  [ WAL Overlay ]   |
+------------------------------------+------------------------------------+
                                     |
                                     v
+-------------------------------------------------------------------------+
|                       B-Tree & Storage Engine                           |
|   [ Table / Index B-Trees ]  --->  [ Overflow Chains ]  ---> [ Freelist]|
+------------------------------------+------------------------------------+
                                     |
                                     v
+-------------------------------------------------------------------------+
|                  Record Decoder & Structural Verifier                   |
|     [ Zero-Copy ValueRef / Row Iterator ]   |   [ 11-Stage Validator ]  |
+------------------------------------+------------------------------------+
                                     |
                                     v
+-------------------------------------------------------------------------+
|                 litewalk CLI  /  Rust Library API                       |
+-------------------------------------------------------------------------+
```

---

## Installation & Platform Setup

**litewalk** runs natively on **Linux**, **macOS**, and **Windows** and requires **Rust 1.75+** (zero C compiler or system library prerequisites).

### 1. Install the Toolchain (if needed)

#### Linux & macOS
```sh
curl --proto '=https' --tlsv1.2 -sSf [https://sh.rustup.rs](https://sh.rustup.rs) | sh
```

#### Windows (PowerShell)
```powershell
winget install Rustlang.Rustup
```

### 2. Install the `litewalk` CLI

```sh
# Install directly from a local checkout
git clone [https://github.com/DushaneW/litewalk.git](https://github.com/DushaneW/litewalk.git)
cd litewalk
cargo install --path .

# Or build a standalone release binary (target/release/litewalk)
cargo build --release --locked
```

### 3. Add `litewalk` as a Library Dependency

To use `litewalk` inside your own Rust project, add it to your `Cargo.toml`:

```toml
[dependencies]
litewalk = { git = "[https://github.com/DushaneW/litewalk.git](https://github.com/DushaneW/litewalk.git)" }
```

---

## CLI Reference

### Quick Examples

```console
$ litewalk info app.db
$ litewalk check app.db --json
$ litewalk page app.db 42 --hex
$ litewalk map app.db
$ litewalk dump app.db users --format json --limit 100 --pretty
$ litewalk forensic app.db --free-space
```

### Subcommands

| Command | Description & Output |
| :--- | :--- |
| `info <db>` | Displays database header fields, page size, encoding, journal mode, and object counts. |
| `schema <db>` | Prints full DDL schema declarations, root pages, columns, affinities, and defaults. |
| `tables <db>` / `indexes <db>` / `objects <db>` | Lists tables, indexes, views, and triggers with their corresponding root page numbers. |
| `page <db> <n> [--hex]` | Decodes page `n`: page type, cell pointers, free space, overflow links, and region-annotated hex dump. |
| `map <db>` | Generates a full ownership map showing which table, index, overflow chain, or freelist owns every page. |
| `check <db>` | Runs the 11-stage structural integrity verifier (returns exit code `1` if corruption is detected). |
| `stats <db> [--cache N]` | Measures page cache hits, misses, and hit ratio across a full database traversal. |
| `wal <db>` | Inspects `<db>-wal` frames, commit boundaries, salt values, and frame checksum integrity. |
| `forensic <db> [--free-space]` | Produces a deep structural report; `--free-space` enumerates unallocated byte regions inside B-tree pages. |
| `dump <db> <table>` | Exports table rows as `TSV`, `CSV`, or `JSON` (`--format`, `--limit`, `--offset`, `--columns`, `--rowid`, `--pretty`, `--hex-blobs`). |
| `get <db> <table> <rowid>` | Performs an $O(\log n)$ B-tree lookup to fetch a single row by its `rowid`. |

### Global Flags & Exit Codes

- **Flags:** `--json` (supported on all subcommands except `dump`, which uses `--format json`), `--with-wal` (overlays committed frames from `<db>-wal`), `--cache <PAGES>`, `--limit <N>`, `--offset <N>`, `--verbose`.
- **Streams & Exit Codes:** Primary data is written to `stdout`; diagnostics and errors go to `stderr`. Returns `0` on success, `1` on runtime error or invalid database (`check`), and `2` on CLI usage errors.

---

## Rust Library API

```rust
use litewalk::Database;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open database file (or use Database::open_with_wal, from_bytes, from_reader)
    let mut db = Database::open("app.db")?;

    // Inspect schema objects
    for table in db.tables()? {
        println!("Table: {} | Columns: {:?}", table.name, table.column_names());
    }

    // Full table scan in rowid order
    let users = db.table("users")?;
    for row in db.scan(&users)? {
        let row = row?;
        println!("rowid={:?} values={:?}", row.rowid, row.values);
    }

    // Fast O(log n) binary-search lookup by rowid
    if let Some(user) = db.get(&users, 4711)? {
        println!("Found user 4711: {:?}", user.values);
    }

    // Full 11-stage structural verification
    let report = db.check()?;
    assert!(report.is_valid());

    Ok(())
}
```

### Advanced Data Access

- **Alternative Sources:** Construct instances via `Database::from_bytes`, `Database::from_reader`, `Database::open_with_wal`, or customize cache bounds via `Database::with_options`.
- **Zero-Allocation Scanning:** Use `Database::for_each_record` to receive borrowed `ValueRef` slices directly from the page buffer without allocating per-row `Vec<Value>` containers.
- **API Docs:** Run `cargo doc --no-deps --open` to browse full rustdoc documentation locally.

---

## Format Compatibility Matrix

Detailed format notes are documented in [`docs/compatibility.md`](docs/compatibility.md).

| SQLite Format Feature | Support Level | Implementation Notes |
| :--- | :---: | :--- |
| **Database Header (100-byte)** | Full | Parses all metadata fields, page sizes (`512`–`65536`), and format versions. |
| **Text Encodings** | Full | `UTF-8`, `UTF-16LE`, and `UTF-16BE` verified against SQLite. |
| **Table B-Trees (`rowid`)** | Full | Full sequential scans and $O(\log n)$ binary-search `rowid` lookups. |
| **Index B-Trees** | Full | Full structural verification and entry scans. |
| **Overflow Page Chains** | Full | Arbitrary-length payload reassembly with cycle and bounds checking. |
| **`WITHOUT ROWID` Tables** | Full (Scan) | Full B-tree traversal and record decoding. |
| **Generated Columns** | `STORED` | `STORED` columns decoded natively; `VIRTUAL` columns require an SQL runtime. |
| **`ALTER TABLE ADD COLUMN` Defaults** | Literals | Literal default values resolved automatically from schema definitions. |
| **Freelist (Trunk & Leaf Pages)** | Full | Complete traversal, accounting, and structural verification. |
| **Auto-Vacuum Databases** | Supported | Safely traverses and validates databases with pointer-map pages. |
| **Write-Ahead Log (`.db-wal`)** | Read-Only | Frame inspection, checksum validation, and committed-frame page overlay. |
| **Write Operations / SQL Execution** | By Design | Read-only format parser—never modifies input files or executes SQL queries. |

---

## Security & Hostile-Input Model

**litewalk** treats every input buffer and database file as potentially malicious:
1. **Strict Read-Only Operation:** Never opens files with write permissions or alters underlying storage.
2. **Bounded Traversals:** Every page pointer, cell offset, overflow chain, and allocation request is strictly range-checked before dereferencing.
3. **Structured Corruption Diagnostics:** Format violations return `Error::Corrupt` with exact page and byte context rather than panicking or hanging.

See [`SECURITY.md`](SECURITY.md) for the complete threat model and vulnerability reporting guidelines.

---

## Verification & Fuzzing Suite

Running `cargo test` executes **70 test suites**, encompassing thousands of generated, mutated, and adversarial database fixtures:

| Test Suite | Coverage & Verification Scope |
| :--- | :--- |
| **Differential (`tests/differential.rs`, `formats.rs`)** | Compares `litewalk` bit-for-bit against `rusqlite` across all page sizes (`512` to `65,536`), testing scans, `rowid` lookups, index entries, `WITHOUT ROWID`, `UTF-16`, `STORED` columns, defaults, and `rowid` aliases. |
| **Integrity (`tests/check.rs`)** | Verifies zero false positives across diverse SQLite-generated databases (deletes, updates, freelists, auto-vacuum, views, triggers) and asserts that **36 targeted binary corruptions** are caught by their exact corresponding check stage. |
| **WAL (`tests/wal.rs`)** | Validates WAL frame headers, salt matching, checksum chains, commit markers, uncommitted frame exclusion, and damaged WAL recovery against SQLite's live view. |
| **CLI (`tests/cli.rs`)** | End-to-end binary verification covering all subcommands, `--json` schema validity, output formatting, and exit status codes. |
| **Fuzz Invariants (`tests/fuzz_invariants.rs`)** | Exercises **6,000 structurally mutated databases**, **4,000 random byte streams**, and all `65,536` two-byte record header prefixes—asserting zero panics, hangs, or runaway allocations. |

### Coverage-Guided Fuzzing (`cargo-fuzz`)

The `fuzz/` directory provides dedicated `libFuzzer` targets (`fuzz_header`, `fuzz_varint`, `fuzz_record`, `fuzz_wal`, and `fuzz_database`) sharing the core invariant assertions:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run fuzz_database -- -max_total_time=600
```

---

## Performance & Benchmarks

Reproduce all benchmarks locally with `cargo run --release --example bench` (methodology documented in [`docs/benchmarking.md`](docs/benchmarking.md)). Baseline measurements below compare `litewalk` against `rusqlite` (`SQLite 3.45` executing prepared statements) on a 2.8 GHz Xeon core:

### 1. Point Lookups by `rowid` (100,000 rows, 100 B payload, 4 KiB pages)

Thanks to $O(\log n)$ binary cell search per page and direct B-tree traversal without SQL statement overhead, `litewalk` performs random `rowid` lookups **~2x faster** than `SELECT * WHERE rowid = ?`:

| Lookups | `litewalk` (No Cache) | `litewalk` (256-Page Cache) | SQLite (`rusqlite`) |
| ---: | ---: | ---: | ---: |
| `1` | `11 µs` total | **`8 µs`** total | `17 µs` total |
| `100` | `733 µs` total | **`624 µs`** total | `1,415 µs` total |
| `1,000` | `4.9 ms` (`4.9 µs` / op) | **`4.4 ms`** (`4.4 µs` / op) | `8.5 ms` (`8.5 µs` / op) |
| `100,000` | `538.1 ms` (`5.4 µs` / op) | **`380.3 ms`** (`3.8 µs` / op) | `842.8 ms` (`8.4 µs` / op) |

### 2. Full Table Scans (100 B payload, 4 KiB pages, 256-page cache)

| Rows | File Size | `litewalk` (`scan` iterator) | `litewalk` (`for_each_record`) | SQLite (`SELECT *`) |
| ---: | ---: | ---: | ---: | ---: |
| `10,000` | `1.1 MiB` | `5.5 ms` | `3.8 ms` | `3.3 ms` |
| `100,000` | `11.5 MiB` | `49.9 ms` | `31.4 ms` | `29.5 ms` |
| `1,000,000` | `115.1 MiB` | `511.3 ms` | `332.3 ms` | `257.5 ms` |

### 3. Payload Scaling (~32 MiB dataset, 4 KiB pages, 256-page cache)

| Payload Size | Rows | File Size | `litewalk` (`scan`) | `litewalk` (`for_each_record`) | SQLite (`SELECT *`) |
| ---: | ---: | ---: | ---: | ---: | ---: |
| `10 B` | `200,000` | `5.3 MiB` | `83.4 ms` | `45.7 ms` | `38.6 ms` |
| `1 KB` | `32,000` | `31.3 MiB` | `34.2 ms` | `26.1 ms` | `20.9 ms` |
| `10 KB` | `3,200` | `31.3 MiB` | `23.0 ms` | `20.4 ms` | `14.6 ms` |
| `100 KB` | `320` | `30.6 MiB` | `22.8 ms` | `22.3 ms` | `12.6 ms` |
| `1 MB` | `32` | `30.6 MiB` | `26.7 ms` | **`19.7 ms`** | `38.1 ms` |

### 4. Page Size Scaling (100,000 rows, 100 B payload, 256-page cache)

| Page Size | File Size | Total Pages | `litewalk` Scan | SQLite Scan | `litewalk` (10k Lookups) | SQLite (10k Lookups) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `512 B` | `12.5 MiB` | `25,503` | `79.9 ms` | `44.7 ms` | **`40.0 ms`** | `88.4 ms` |
| `1,024 B` | `12.3 MiB` | `12,628` | `67.5 ms` | `34.4 ms` | **`38.8 ms`** | `71.9 ms` |
| `2,048 B` | `11.6 MiB` | `5,914` | `56.1 ms` | `35.8 ms` | **`36.5 ms`** | `73.0 ms` |
| `4,096 B` | `11.5 MiB` | `2,937` | `52.0 ms` | `25.7 ms` | **`36.0 ms`** | `75.7 ms` |
| `8,192 B` | `11.4 MiB` | `1,460` | `50.5 ms` | `25.2 ms` | **`50.2 ms`** | `77.5 ms` |
| `16,384 B` | `11.3 MiB` | `725` | `48.9 ms` | `24.6 ms` | **`64.7 ms`** | `93.4 ms` |
| `32,768 B` | `11.3 MiB` | `362` | `49.7 ms` | `22.6 ms` | **`64.8 ms`** | `107.4 ms` |
| `65,536 B` | `11.4 MiB` | `182` | `51.9 ms` | `21.6 ms` | **`33.5 ms`** | `142.4 ms` |

### 5. Page Cache Capacity (100,000 rows, 2,937 pages @ 4 KiB, 100,000 random lookups)

| Cache Capacity (Pages) | 100k Random Lookups | Cache Hit Ratio | Full Table Scan |
| ---: | ---: | ---: | ---: |
| `0` | `533.6 ms` | `0.0%` | `52.5 ms` |
| `64` | `340.0 ms` | `67.3%` | `48.1 ms` |
| `256` | `356.7 ms` | `69.5%` | `48.3 ms` |
| `1,024` | `352.5 ms` | `78.3%` | `50.6 ms` |
| `4,096` | **`276.7 ms`** | **`99.0%`** | `50.3 ms` |

---

## Development & Repository Layout

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo doc --no-deps
```

- `src/` — Core format parser, B-tree traversal engine, WAL reader, validator, and `src/cli/` binary implementation.
- `tests/` — Differential, integrity, WAL, CLI, and mutation-invariants test suites (`tests/common/` houses the SQLite fixture generator).
- `fuzz/` — `cargo-fuzz` / `libFuzzer` harnesses.
- `examples/bench.rs` — Reproducible benchmark runner.
- `docs/` — [Architecture Guide](docs/architecture.md), [Compatibility Reference](docs/compatibility.md), and [Benchmarking Methodology](docs/benchmarking.md).

---

## Specifications & References

- [SQLite Database File Format Specification](https://www.sqlite.org/fileformat.html)
- [Write-Ahead Logging (WAL) Format](https://www.sqlite.org/walformat.html)
- [SQLite Record Payload Format](https://www.sqlite.org/fileformat2.html#record_format)
- [Datatypes & Column Affinity Rules](https://www.sqlite.org/datatype3.html)

---

## License

Licensed under the [**MIT License**](LICENSE).
