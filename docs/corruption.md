# Corruption handling and `litewalk check`

Every database file is untrusted input. The reader never trusts a number it reads from the file:
page numbers are range-checked before use, sizes are compared with what can fit in the file,
offsets are bounds-checked, loops have bounds, and recursion is replaced by an explicit stack.

## Error kinds

`CorruptionKind` names the damaged structure: `Header`, `Truncated`, `PageNumber`, `PageType`,
`CellPointer`, `Varint`, `Record`, `Overflow`, `BTree`, `Freelist`, `Schema`, `Wal`. Errors also
carry page, offset, cell, rowid, table, depth and expected/actual values when they are known.

```
corrupt database: overflow page points to itself (table t, page 183, cell 3, rowid 7)
```

## What `check` verifies

| section | checks |
|---|---|
| SQLite header | magic, page size, reserved bytes, payload fractions, encoding, schema format, counts |
| page boundaries | file length vs page count; every child, root, overflow and freelist pointer is inside the database |
| page types | each page in a b-tree has a valid type byte, and the right family (table vs index) |
| cell pointers | pointer array fits; each cell offset is in the content area; cells do not overlap each other or free blocks; fragmented bytes <= 60; the free block chain is ordered and terminates |
| B-tree structure | no page shared between trees, all leaves at one depth, non-root interior pages have cells |
| B-tree ordering | table rowids strictly increasing and inside the key range of every ancestor |
| overflow chains | complete, acyclic, in range, ending with 0, pages not shared |
| freelist | trunk chain acyclic, leaf counts plausible, header count equals what the walk finds |
| schema | readable rows, known types, unique names, root pages valid, indexes refer to existing tables |
| record payloads | every record's header and bodies fit its payload |
| page usage | every page is owned by exactly one structure (a b-tree, an overflow chain, the freelist, a pointer map or the lock byte) |

"Page usage" is skipped when an earlier problem means ownership cannot be established.

## What `check` does not verify

* Index key order and uniqueness (needs collations).
* That an index contains exactly the rows of its table.
* Pointer-map entry contents, free block *contents*, or the `-wal`'s consistency with the
  database.
* Anything about SQL: constraints, triggers, view definitions.

SQLite's own `PRAGMA integrity_check` covers some of these. litewalk's check is a structural check
of the file, not a replacement.

## Test corpus

`tests/common/corpus.rs` builds a valid 512-byte-page database with a known layout and applies 36
single-purpose patches (31 byte patches plus 5 schema lies made with `writable_schema`) (header fields, page types, cell pointers, free block cycles, child
pointers, overflow cycles and self-references, freelist damage, a schema that lies about root
pages). Each case asserts that `check` reports an issue in the *expected section*, and then reads
the corrupt file through every API to confirm there is no panic or hang. The corpus is generated
in memory at test time rather than committed as binary files, so it can be regenerated and
reviewed as code.

Randomised testing (`tests/fuzz_invariants.rs` and the `fuzz/` targets) is described in the
README.
