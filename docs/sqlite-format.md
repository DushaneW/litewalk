# The SQLite file format, as litewalk reads it

Authoritative reference: <https://www.sqlite.org/fileformat.html>. This page describes only what
litewalk implements and how.

## File and pages

A database is a sequence of fixed-size pages numbered from 1. The page size is a power of two from
512 to 65536 (stored as 1 for 65536 because the field has 16 bits). The last `reserved` bytes of
each page are not available to SQLite's own structures; `usable = page_size - reserved`, and every
size calculation uses `usable`.

Page 1 starts with the 100-byte database header, and its b-tree page header follows at offset 100.

## Header fields litewalk reads

| offset | size | field |
|---:|---:|---|
| 0 | 16 | magic `SQLite format 3\0` |
| 16 | 2 | page size |
| 18, 19 | 1, 1 | file format write / read version (2 = WAL) |
| 20 | 1 | reserved bytes per page |
| 21-23 | 1 each | payload fractions, must be 64, 32, 32 |
| 24 | 4 | file change counter |
| 28 | 4 | database size in pages |
| 32, 36 | 4, 4 | first freelist trunk page, number of freelist pages |
| 40, 44 | 4, 4 | schema cookie, schema format |
| 52 | 4 | largest root page (non-zero means auto-vacuum) |
| 56 | 4 | text encoding: 1 UTF-8, 2 UTF-16le, 3 UTF-16be |
| 60 | 4 | user version |
| 64 | 4 | incremental vacuum flag |
| 68 | 4 | application id |
| 92 | 4 | version-valid-for |
| 96 | 4 | SQLite version that last wrote the file |

The in-header page count is used only when the change counter (24) equals version-valid-for (92);
otherwise the page count is `file length / page size`, as SQLite does. A file shorter than the
page count requires is rejected as truncated.

## Page kinds

A page is one of: a b-tree page, an overflow page, a freelist trunk or leaf page, an auto-vacuum
pointer-map page, or the lock-byte page. Only b-tree pages carry a type byte; every other kind is
recognised by being referenced from somewhere (`litewalk map`).

* **Freelist.** Trunk page: next trunk (4 bytes), leaf count (4), then that many leaf page numbers.
* **Pointer map** (auto-vacuum). Pages 2, 2 + n, 2 + 2n, ... where `n = usable / 5 + 1`.
  litewalk identifies them; it does not decode their entries.
* **Lock byte.** The page containing byte offset 0x40000000 is never used. This is implemented as
  specified but not exercised by tests, because it needs a database larger than 1 GiB.

## Schema

Page 1 is the root of a table b-tree, `sqlite_schema`, with columns
`type, name, tbl_name, rootpage, sql`. Every table and index has a root page recorded there.
Views and triggers have root page 0. The `sql` text is stored in the database's text encoding.

litewalk tokenizes that SQL text for column names, declared types, `PRIMARY KEY`, `NOT NULL`,
`DEFAULT`, generated columns, `WITHOUT ROWID`, and index column lists. See
[compatibility.md](compatibility.md) for what this covers and what it does not.
