# Write-ahead log

In WAL mode, committed changes are appended to `<db>-wal` and only later copied (checkpointed)
into the main file. A reader that ignores the WAL sees an older database.

## Format

```
WAL header, 32 bytes:  magic | version | page size | checkpoint seq | salt1 | salt2 | checksum1 | checksum2
frame:                 24-byte header (page number | db size if commit, else 0 | salt1 | salt2 | checksum1 | checksum2)
                       + one page
```

The magic (0x377f0682 or 0x377f0683) selects whether checksum words are read little- or
big-endian. The checksum is a running Fletcher-style sum over 32-bit word pairs; each frame's
checksum covers the first 8 bytes of its header and its page, seeded with the previous frame's
checksum (the first frame is seeded with the header's). A frame is valid only if its salts equal
the header's and its checksum matches. The first invalid frame ends the log; everything after it
is stale data from an earlier generation of the file.

A transaction is committed when its last frame has a non-zero "database size after commit".
Frames after the last commit frame are uncommitted and ignored by readers.

## What litewalk does

* `Wal::open` / `Wal::from_bytes` validate the header (magic, version, page size, checksum) and
  walk frames, reporting why parsing stopped (`WalStop`).
* `litewalk wal <db>` prints the header, frame counts, commit boundaries and, with `--verbose`,
  every frame.
* `Database::open_with_wal`, `Database::with_wal` and `--with-wal` overlay the **latest committed**
  version of each page from the WAL on the main file, so reads see what a SQLite reader would see.
  The main file's page size must match the WAL's.

## What it does not do

* It never writes either file, never checkpoints and never replays anything onto disk.
* It does not read the `-shm` wal-index. The WAL is scanned from the start every time, so the
  result reflects the files as they are, not a reader's snapshot at some earlier `mxFrame`.
* A database being written concurrently can be read inconsistently (a frame may be half written).
  Copy the files first, or use SQLite's backup API.
* Checksums use the byte order from the magic; WAL files from unusual platforms are untested.

## Verification

`tests/wal.rs` uses WAL files written by SQLite: every frame of real logs must validate, commit
boundaries must match, and the overlay's table scans, index scans and integrity check must equal
what SQLite itself reports for the live database. A transaction that spilled uncommitted frames
into the log must be parsed but not applied. Corrupted logs end the log at the damaged frame.
