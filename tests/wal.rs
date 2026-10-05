//! WAL parsing and overlay, checked against WAL files that SQLite itself wrote.

mod common;

use common::*;
use litewalk::{Database, Error, Wal, WalStop};
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use std::path::PathBuf;

fn wal_path(db: &std::path::Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push("-wal");
    p.into()
}

/// A WAL-mode database whose WAL is not checkpointed, and a snapshot copy of both files.
struct Fixture {
    _live: TempDb,
    conn: Connection,
    snapshot: TempDb,
}

impl Fixture {
    fn new(page_size: u32) -> Self {
        let live = TempDb::new();
        let conn = live.connect();
        conn.pragma_update(None, "page_size", page_size).unwrap();
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))
            .unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        Fixture {
            _live: live,
            conn,
            snapshot: TempDb::new(),
        }
    }

    /// Copies the database and WAL as they are right now.
    fn snapshot(&self) -> &TempDb {
        std::fs::copy(self._live.path(), self.snapshot.path()).unwrap();
        std::fs::copy(wal_path(self._live.path()), wal_path(self.snapshot.path())).unwrap();
        &self.snapshot
    }
}

fn scan(db: &mut Database<std::fs::File>, name: &str) -> Vec<Vec<litewalk::Value>> {
    let t = db.table(name).unwrap();
    db.scan(&t).unwrap().map(|r| r.unwrap().values).collect()
}

#[test]
fn frames_checksums_and_commit_markers_match_what_sqlite_wrote() {
    for page_size in [512u32, 4096, 65536] {
        let f = Fixture::new(page_size);
        f.conn
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        let mut rng = Rng(u64::from(page_size));
        for round in 0..5i64 {
            f.conn.execute_batch("BEGIN").unwrap();
            for i in 0..60 {
                insert(
                    &f.conn,
                    "INSERT OR REPLACE INTO t VALUES (?,?)",
                    vec![
                        SqlValue::Integer(round * 40 + i),
                        SqlValue::Text(gen_text(&mut rng)),
                    ],
                );
            }
            f.conn.execute_batch("COMMIT").unwrap();
        }
        let snap = f.snapshot();
        let wal = Wal::open(wal_path(snap.path())).unwrap();
        let frame_size = 24 + page_size as u64;
        let expected_frames =
            (std::fs::metadata(wal_path(snap.path())).unwrap().len() - 32) / frame_size;

        assert_eq!(wal.header.page_size, page_size);
        assert_eq!(
            wal.frames.len() as u64,
            expected_frames,
            "every frame SQLite wrote must validate (page size {page_size})"
        );
        assert_eq!(wal.stop, WalStop::EndOfFile);
        assert_eq!(wal.trailing_bytes, 0);
        assert_eq!(
            wal.committed_frames,
            wal.frames.len(),
            "all transactions were committed"
        );
        let pages: i64 = f
            .conn
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        assert_eq!(i64::from(wal.db_size_after_commit.unwrap()), pages);
        assert!(
            wal.frames
                .iter()
                .filter(|fr| fr.commit_size.is_some())
                .count()
                >= 6,
            "one commit frame per transaction"
        );
        assert!(
            wal.committed_pages() < wal.frames.len(),
            "rewritten pages appear in several frames"
        );
    }
}

#[test]
fn overlaying_the_wal_shows_what_a_sqlite_reader_sees() {
    let f = Fixture::new(1024);
    f.conn
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, b BLOB); CREATE INDEX t_v ON t(v);",
        )
        .unwrap();
    let mut rng = Rng(3);
    f.conn.execute_batch("BEGIN").unwrap();
    for i in 0..500i64 {
        insert(
            &f.conn,
            "INSERT INTO t VALUES (?,?,?)",
            vec![
                SqlValue::Integer(i),
                SqlValue::Text(gen_text(&mut rng)),
                SqlValue::Blob(gen_blob(&mut rng)),
            ],
        );
    }
    f.conn.execute_batch("COMMIT").unwrap();
    // Later commits rewrite pages that earlier frames already hold: the newest frame must win.
    f.conn.execute_batch("UPDATE t SET v = 'updated' WHERE id % 5 = 0; DELETE FROM t WHERE id % 7 = 0; INSERT INTO t VALUES (1000, 'late', NULL);").unwrap();

    let snap = f.snapshot();
    let expected = query_values(&f.conn, "SELECT * FROM t ORDER BY id", 3);

    let mut db = Database::open_with_wal(snap.path()).unwrap();
    assert_same_rows(&scan(&mut db, "t"), &expected, "WAL overlay");
    let report = db.check().unwrap();
    assert!(report.is_valid(), "{:#?}", report.issues);
    let ix = db.index("t_v").unwrap();
    let entries: Vec<_> = db
        .scan_index(&ix)
        .unwrap()
        .map(|e| e.unwrap().values)
        .collect();
    assert_same_rows(
        &entries,
        &query_values(
            &f.conn,
            "SELECT v, id FROM t INDEXED BY t_v ORDER BY v, id",
            2,
        ),
        "index through the WAL",
    );

    // Without the overlay the main file is a stale (here: nearly empty) database.
    let stale = Database::open(snap.path()).and_then(|mut d| {
        d.table("t")
            .and_then(|t| d.scan(&t).map(|s| s.filter(|r| r.is_ok()).count()))
    });
    match stale {
        Err(_) => {}
        Ok(n) => assert!(
            n < expected.len(),
            "main file alone must not contain the WAL's rows ({n})"
        ),
    }
}

#[test]
fn uncommitted_frames_are_parsed_but_not_applied() {
    let f = Fixture::new(512);
    f.conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'committed');").unwrap();
    // A tiny page cache forces SQLite to spill uncommitted pages into the WAL.
    f.conn.pragma_update(None, "cache_size", 5).unwrap();
    f.conn.execute_batch("BEGIN").unwrap();
    for i in 2..400i64 {
        f.conn
            .execute(
                "INSERT INTO t VALUES (?1, ?2)",
                rusqlite::params![i, "u".repeat(400)],
            )
            .unwrap();
    }
    let snap = f.snapshot(); // taken in the middle of the transaction
    let wal = Wal::open(wal_path(snap.path())).unwrap();
    assert!(
        wal.committed_frames < wal.frames.len(),
        "expected spilled, uncommitted frames: {} of {}",
        wal.committed_frames,
        wal.frames.len()
    );

    let mut db = Database::open_with_wal(snap.path()).unwrap();
    let rows = scan(&mut db, "t");
    assert_eq!(rows.len(), 1, "only the committed row is visible");
    assert_eq!(rows[0][1], litewalk::Value::Text("committed".into()));
    f.conn.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn damaged_wals_end_the_log_or_are_rejected() {
    let f = Fixture::new(1024);
    f.conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (3);").unwrap();
    let snap = f.snapshot();
    let bytes = std::fs::read(wal_path(snap.path())).unwrap();
    let good = Wal::from_bytes(bytes.clone()).unwrap();
    assert!(good.frames.len() >= 4);
    let frame_size = 24 + 1024;

    // flip a data byte in frame 2: frames 0 and 1 survive
    let mut b = bytes.clone();
    b[32 + 2 * frame_size + 24 + 7] ^= 0xff;
    let w = Wal::from_bytes(b).unwrap();
    assert_eq!((w.frames.len(), w.stop), (2, WalStop::ChecksumMismatch));
    assert!(w.committed_frames <= 2);

    // truncate in the middle of the last frame
    let w = Wal::from_bytes(bytes[..bytes.len() - 10].to_vec()).unwrap();
    assert_eq!(
        (w.frames.len(), w.stop),
        (good.frames.len() - 1, WalStop::TruncatedFrame)
    );

    // header damage is fatal
    for (at, what) in [
        (0usize, "magic"),
        (9, "page size"),
        (17, "salt"),
        (26, "checksum"),
    ] {
        let mut b = bytes.clone();
        b[at] ^= 0x40;
        assert!(Wal::from_bytes(b).is_err(), "{what}");
    }
    assert!(matches!(
        Wal::from_bytes(vec![0; 10]),
        Err(Error::Corrupt(_))
    ));

    // WAL page size must equal the database's
    let other = Fixture::new(4096);
    other
        .conn
        .execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1);")
        .unwrap();
    let s2 = other.snapshot();
    let mismatched = Wal::open(wal_path(s2.path())).unwrap();
    let file = std::fs::File::open(snap.path()).unwrap();
    assert!(Database::with_wal(file, &mismatched, litewalk::Options::default()).is_err());
}

#[test]
fn random_wal_corruption_never_panics() {
    let f = Fixture::new(512);
    f.conn
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v); CREATE INDEX i ON t(v);")
        .unwrap();
    let mut rng = Rng(42);
    for i in 0..100i64 {
        insert(
            &f.conn,
            "INSERT INTO t VALUES (?,?)",
            vec![SqlValue::Integer(i), SqlValue::Text(gen_text(&mut rng))],
        );
    }
    let snap = f.snapshot();
    let wal_bytes = std::fs::read(wal_path(snap.path())).unwrap();
    let db_bytes = std::fs::read(snap.path()).unwrap();
    for _ in 0..1500 {
        let mut w = wal_bytes.clone();
        for _ in 0..1 + rng.below(3) {
            let at = rng.below(w.len() as u64) as usize;
            w[at] = rng.next() as u8;
        }
        let Ok(wal) = Wal::from_bytes(w) else {
            continue;
        };
        let Ok(mut db) = Database::with_wal(
            std::io::Cursor::new(db_bytes.clone()),
            &wal,
            litewalk::Options::default(),
        ) else {
            continue;
        };
        let _ = db.check();
        if let Ok(tables) = db.tables() {
            for t in &tables {
                if let Ok(s) = db.scan(t) {
                    assert!(s.take(100_000).count() < 100_000);
                }
            }
        }
    }
}
