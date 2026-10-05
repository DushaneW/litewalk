# Overflow pages

A payload that does not fit on its b-tree page keeps a prefix there and the rest in a chain of
overflow pages.

```
b-tree cell:  [ header ][ local part of payload ][ 4-byte first overflow page ]
overflow:     [ 4-byte next page (0 = last) ][ up to usable - 4 bytes of payload ]
```

## How much stays local

With `U` the usable page size and `P` the payload size:

```
table leaf:  X = U - 35
index:       X = (U - 12) * 64 / 255 - 23
all kinds:   M = (U - 12) * 32 / 255 - 23

if P <= X:             everything is local
else K = M + (P - M) % (U - 4)
     local = K if K <= X else M
```

Both formulas are unit-tested against hand computations, and every page size from 512 to 65536
is exercised against SQLite in `tests/differential.rs` with payloads up to 100 KB (so a mistake in
either formula produces wrong data, and does).

## What litewalk rejects

* a payload size larger than `page_count * usable` (nothing that large fits in the file; this
  bound also prevents huge allocations from a corrupt length);
* a chain pointer outside the database, a page that points to itself, a cycle, or a chain that
  ends (pointer 0) before the payload is complete;
* in `check` only: a last overflow page whose next pointer is not 0, and an overflow page that
  is also used by something else.

A cell with no overflow borrows its bytes from the cached page; only overflowing payloads are
copied into one buffer.
