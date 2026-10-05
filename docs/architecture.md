# Architecture

litewalk reads a SQLite database by interpreting its bytes. Nothing links against SQLite and
nothing is written.

```
SQLite file (+ optional -wal)
    |
Header         src/header.rs     100 bytes: page size, encoding, counts, freelist
    |
Pager          src/pager.rs      read page N; LRU cache; WAL overlay
    |
B-tree         src/btree.rs      page header, cell pointers, cells, overflow chains
    |
Cells          src/btree.rs      table leaf / table interior / index leaf / index interior
    |
Payload        src/btree.rs      local bytes + overflow chain -> one buffer (or a page slice)
    |
Record         src/record.rs     header of serial types, column bodies, text encoding
    |
Values         src/record.rs     Value / ValueRef
    |
Rows / entries src/db.rs         schema layout: rowid alias, defaults, REAL affinity,
                                 WITHOUT ROWID column order
```

Alongside the read path:

| module | purpose |
|---|---|
| `src/schema.rs` | tokenizer for `CREATE TABLE` / `CREATE INDEX` text: columns, types, defaults, generated columns, primary keys |
| `src/check.rs` | one walk over every b-tree, the freelist and the special pages; produces both the integrity report and the page map |
| `src/inspect.rs` | decodes one page (cells, free blocks, regions) and hex dumps |
| `src/forensic.rs` | read-only structural report and free-region listing built on `check` and `inspect` |
| `src/wal.rs` | WAL header and frame parsing, checksum validation, committed-frame overlay |
| `src/cli/` | the `litewalk` binary: argument parsing, JSON writer, one module per command group |

## Decisions worth knowing

**Errors carry context.** `Error::Corrupt` holds a `Corruption` with a kind, a reason and optional
page, offset, cell, rowid, table, depth, expected and actual. The code that detects a problem
fills what it knows; callers further up add the rest with `with_table`, `with_rowid`, etc. Fields
set by an inner layer are never overwritten. `CorruptionKind` also groups findings into the
sections of `litewalk check`.

**One traversal, two consumers.** `check` and `map` share `check::analyze`, so a page cannot be
reported as belonging to two different things.

**Scans are iterative.** `TreeScan` keeps an explicit stack, so depth is bounded by a constant
(`MAX_DEPTH = 64`) rather than by the call stack, and every page may be visited once, which turns
cycles and shared pages into errors instead of infinite loops.

**Lookups decode O(log n) cells.** `Database::get` uses `PageView`, which reads cells on demand,
instead of `load_node`, which decodes a whole page. See [benchmarking.md](benchmarking.md).

**`&mut self` everywhere.** Reading needs `Seek` on the source and updates the cache, so the
database handle is `&mut`. Interior mutability would allow `&self` methods but would make the
type `!Sync`, hide I/O behind shared references, and let two iterators interleave on one handle.

**No SQL.** The schema reader tokenizes; it does not parse or evaluate expressions. Features that
would need an expression evaluator (virtual generated columns, non-literal `DEFAULT`s) are
reported as unsupported instead of approximated.

**No unsafe.** `#![forbid(unsafe_code)]` is set through `[lints.rust]`.
