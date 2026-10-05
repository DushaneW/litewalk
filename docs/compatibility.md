# Compatibility

Status as tested. "Tested" means a test compares litewalk with SQLite 3.45 (via `rusqlite`
0.31, bundled) or, for corruption, asserts the specific detection.

| Feature | Status | Notes |
|---|---|---|
| Header | Supported | all fields in [sqlite-format.md](sqlite-format.md); tested at all 8 page sizes |
| Page sizes 512-65536, reserved bytes | Supported | reserved bytes parsed and used in all size formulas; no test database has reserved bytes > 0 |
| UTF-8 | Supported | strict decoding |
| UTF-16le / UTF-16be | Supported | rows, schema, index entries; tested against SQLite |
| Table b-trees | Supported | |
| Index b-trees | Supported | structure and scans tested; key order not verified; automatic indexes listed without columns |
| Overflow pages | Supported | payloads to 100 KB tested at every page size |
| Records, all serial types | Supported | |
| `INTEGER PRIMARY KEY` rowid alias | Supported | exact SQLite rules |
| REAL affinity conversion | Supported | found and fixed by differential testing |
| `WITHOUT ROWID` | Supported | scan only (no key lookup); tested with composite and DESC keys |
| Generated columns, STORED | Supported | read from the record |
| Generated columns, VIRTUAL | Reported unsupported | no SQL engine to evaluate the expression |
| `ALTER TABLE ADD COLUMN` defaults | Partly | literals with affinity applied; expressions and ambiguous conversions reported unsupported |
| Freelist | Supported | walked and checked; leaf contents not interpreted |
| Auto-vacuum | Partly | databases read and checked; pointer-map pages identified but entries not decoded |
| Lock-byte page | Implemented, untested | needs a database over 1 GiB |
| WAL | Supported (read-only) | inspection and committed-frame overlay; see [wal.md](wal.md) |
| Rollback journal (`-journal`) | Not supported | a hot journal means the main file may be inconsistent; litewalk does not look at it |
| Virtual tables, FTS, R-Tree | Listed, not read | shadow tables are ordinary tables and can be read |
| Encrypted databases (SEE, SQLCipher) | Not supported | they do not start with the magic string |
| Page cache | Supported | LRU, configurable, hit/miss statistics |
| Integrity checking | Supported | see [corruption.md](corruption.md) for what it does not cover |
| Large files | Supported | 64-bit offsets; page numbers are 32-bit as in SQLite |
| Writing, repair, vacuum | Not supported, by design | |
| SQL | Not supported, by design | |

## Minimum Rust version

1.75 (set in `Cargo.toml`, tested in CI).

## Platforms

CI builds and tests on Linux, macOS and Windows. Release binaries are built for the targets in
`.github/workflows/release.yml`; those builds have not been exercised by the author on every
target.
