# B-trees

Every table and index is a b-tree whose nodes are pages. litewalk supports all four page types.

| byte | type | stores |
|---:|---|---|
| 0x05 | table interior | child page numbers and rowid keys |
| 0x0d | table leaf | rowids and row payloads |
| 0x02 | index interior | child page numbers and index records |
| 0x0a | index leaf | index records |

## Page layout

```
[ page header ][ cell pointer array ][ unallocated ][ ... cells, growing downward ... ]
 8 or 12 bytes   2 bytes per cell                      cell content area
```

Page header: type (1), first free block (2), cell count (2), start of the cell content area (2,
0 means 65536), fragmented free bytes (1), and for interior pages the rightmost child (4).
Cells are written from the end of the page toward the start; the pointer array grows from the
other side. Space freed by deletions becomes a *free block* (a chain starting at the header's
first-free-block field) or, if smaller than 4 bytes, *fragmented bytes*.

## Cells

```
table leaf       varint payload size | varint rowid | payload | [4-byte overflow page]
table interior   4-byte left child   | varint rowid key
index leaf       varint payload size | payload | [4-byte overflow page]
index interior   4-byte left child   | varint payload size | payload | [4-byte overflow page]
```

A table interior cell's key is the largest rowid in its left subtree; the rightmost child holds
everything larger. An index interior cell holds a real index entry that sorts between its left
child and the next one, so index scans emit interior cells in order between children.

## Ordering and what is checked

* Table b-trees are sorted by rowid. `scan` fails if rowids do not increase; `check` additionally
  verifies each rowid against the key range its ancestors allow.
* Index b-trees are sorted by their records under SQLite's comparison rules, which depend on
  collations. litewalk does **not** verify index key order; it verifies structure only.
* Every leaf must be at the same depth.

## Traversal safety

Each page may be visited once per traversal (a repeated page is an error), depth is capped at 64,
and traversal is iterative. A page with zero cells is legal only as an interior page that has just
a rightmost pointer (SQLite produces one as the root of the schema table).
