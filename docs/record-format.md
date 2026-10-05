# Records

A cell's payload (table rows and index entries alike) is a *record*:

```
varint header size (counts itself) | serial type varints ... | column bodies ...
```

## Varints

1 to 9 bytes, big-endian, 7 bits per byte with the high bit meaning "more follows"; the ninth
byte, if reached, contributes all 8 bits. `read_varint` returns `None` on truncation. SQLite
never writes non-minimal varints, but readers accept them; litewalk does too.

## Serial types

| code | meaning | body bytes |
|---:|---|---:|
| 0 | NULL | 0 |
| 1, 2, 3, 4 | signed integer, 8 / 16 / 24 / 32 bit | 1, 2, 3, 4 |
| 5 | signed integer, 48 bit | 6 |
| 6 | signed integer, 64 bit | 8 |
| 7 | IEEE 754 double, big-endian | 8 |
| 8, 9 | the integers 0 and 1 | 0 |
| 10, 11 | reserved: rejected as corruption | - |
| N >= 12, even | BLOB | (N - 12) / 2 |
| N >= 13, odd | TEXT | (N - 13) / 2 |

Integers are sign-extended from their stored width.

## Text encodings

The database encoding applies to all text, including the schema. UTF-8 is decoded strictly
(invalid bytes are an error, not replaced). UTF-16le/be is decoded with `char::decode_utf16`;
odd lengths and lone surrogates are errors.

## From record to row

A stored record is not always a logical row:

* **Rowid alias.** A column declared exactly `INTEGER PRIMARY KEY` (not `INT`, not `... DESC`) is
  stored as NULL; its value is the rowid.
* **REAL affinity.** SQLite stores integral REAL values as integers to save space and converts
  them back on read. litewalk does the same for columns whose declared type has REAL affinity.
* **Short records.** `ALTER TABLE ... ADD COLUMN` does not rewrite rows, so older rows have fewer
  columns. Missing columns take the column's literal `DEFAULT`, with the column's affinity applied.
  Non-literal defaults produce `Error::Unsupported`, and so do literals where affinity conversion
  would require number parsing rules litewalk does not reimplement (for example `'12'` on an
  INTEGER column).
* **WITHOUT ROWID.** The record stores the primary key columns first, in key order, followed by
  the remaining columns in declaration order. litewalk puts them back in declaration order.
* **Generated columns.** STORED columns are in the record like any other; VIRTUAL columns are not,
  so tables that have them are reported as unsupported.

`Database::for_each_record` returns stored records unchanged (borrowed, no per-value allocation),
without any of the adjustments above.
