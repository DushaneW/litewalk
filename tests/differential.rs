//! Every result is compared with what SQLite itself returns for the same file.

mod common;

use common::*;
use litewalk::{Database, Error, Value};
use rusqlite::types::Value as SqlValue;

const PAGE_SIZES: [u32; 8] = [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];

fn assert_table_matches(db: &mut Database<std::fs::File>, conn: &rusqlite::Connection, name: &str) {
    let table = db.table(name).unwrap();
    let expected = reference_rows(conn, name, table.columns.len());
    let actual: Vec<_> = db.scan(&table).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(actual.len(), expected.len(), "row count of {name}");
    for (a, (rowid, vals)) in actual.iter().zip(&expected) {
        assert_eq!(a.rowid, Some(*rowid), "rowid in {name}");
        assert_eq!(
            a.values.len(),
            vals.len(),
            "column count in {name} row {rowid}"
        );
        for (i, (x, y)) in a.values.iter().zip(vals).enumerate() {
            assert!(
                same(x, y),
                "{name} rowid {rowid} column {i}: {x:?} != {y:?}"
            );
        }
    }
}

fn populate(conn: &rusqlite::Connection, rng: &mut Rng, rows: u32) {
    conn.execute_batch(
        "CREATE TABLE t1 (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d BLOB, e);
         CREATE TABLE t2 (x, y);
         CREATE TABLE t3 (z);
         CREATE INDEX t1_a ON t1(a);",
    )
    .unwrap();
    conn.execute_batch("BEGIN").unwrap();
    let mut id: i64 = rng.below(1000) as i64 - 500;
    for _ in 0..rows {
        id += 1 + rng.below(5) as i64; // gaps, so the tree is not dense
        insert(
            conn,
            "INSERT INTO t1 VALUES (?,?,?,?,?,?)",
            vec![
                SqlValue::Integer(id),
                if rng.chance(10) {
                    SqlValue::Null
                } else {
                    SqlValue::Integer(gen_int(rng))
                },
                if rng.chance(10) {
                    SqlValue::Null
                } else {
                    SqlValue::Real(gen_real(rng))
                },
                if rng.chance(10) {
                    SqlValue::Null
                } else {
                    SqlValue::Text(gen_text(rng))
                },
                if rng.chance(10) {
                    SqlValue::Null
                } else {
                    SqlValue::Blob(gen_blob(rng))
                },
                gen_any(rng),
            ],
        );
        insert(
            conn,
            "INSERT INTO t2 VALUES (?,?)",
            vec![gen_any(rng), gen_any(rng)],
        );
    }
    conn.execute_batch("COMMIT").unwrap();
    // Deleting and growing rows leaves free pages and fragmented leaves behind.
    conn.execute_batch("DELETE FROM t1 WHERE rowid % 3 = 0; DELETE FROM t2 WHERE rowid % 4 = 1;")
        .unwrap();
    conn.execute("UPDATE t1 SET c = ?1 WHERE rowid % 7 = 0", [gen_text(rng)])
        .unwrap();
}

#[test]
fn full_scans_match_sqlite_for_every_page_size() {
    for (n, page_size) in PAGE_SIZES.into_iter().enumerate() {
        let tmp = TempDb::new();
        let conn = tmp.connect();
        conn.pragma_update(None, "page_size", page_size).unwrap();
        conn.pragma_update(None, "user_version", 42).unwrap();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ n as u64);
        populate(&conn, &mut rng, 1500);

        let mut db = Database::open(tmp.path()).unwrap();
        for t in ["t1", "t2", "t3"] {
            assert_table_matches(&mut db, &conn, t);
        }

        let h = db.header();
        let pragma = |name: &str| {
            conn.pragma_query_value(None, name, |r| r.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(h.page_size as i64, pragma("page_size"));
        assert_eq!(
            i64::from(h.page_count),
            pragma("page_count"),
            "page size {page_size}"
        );
        assert_eq!(i64::from(h.freelist_count), pragma("freelist_count"));
        assert_eq!(i64::from(h.user_version), pragma("user_version"));
        assert_eq!(i64::from(h.schema_cookie), pragma("schema_version"));
        assert!(!h.wal());
    }
}

#[test]
fn lookups_by_rowid_match_sqlite() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.pragma_update(None, "page_size", 1024).unwrap();
    let mut rng = Rng(7);
    populate(&conn, &mut rng, 3000);

    let mut db = Database::open(tmp.path()).unwrap();
    for name in ["t1", "t2"] {
        let table = db.table(name).unwrap();
        let ncols = table.columns.len();
        let mut stmt = conn
            .prepare(&format!("SELECT rowid, * FROM {name} WHERE rowid = ?1"))
            .unwrap();
        // Every rowid in the populated range, so large overflow rows are always probed.
        let probes = (-600..9000).chain([i64::MIN, i64::MAX]);
        let (mut hits, mut misses) = (0, 0);
        for rowid in probes {
            let expected: Option<(i64, Vec<Value>)> = stmt
                .query_map([rowid], |row| {
                    Ok((
                        row.get(0)?,
                        (0..ncols)
                            .map(|i| from_sql(row.get_ref(i + 1).unwrap()))
                            .collect(),
                    ))
                })
                .unwrap()
                .next()
                .transpose()
                .unwrap();
            let actual = db.get(&table, rowid).unwrap();
            match (actual, expected) {
                (None, None) => misses += 1,
                (Some(a), Some((id, vals))) => {
                    hits += 1;
                    assert_eq!(a.rowid, Some(id));
                    assert!(
                        a.values.iter().zip(&vals).all(|(x, y)| same(x, y)),
                        "{name} rowid {rowid}"
                    );
                }
                (a, e) => panic!("{name} rowid {rowid}: litewalk {a:?}, sqlite {e:?}"),
            }
        }
        assert!(
            hits > 1000 && misses > 1000,
            "probe mix is degenerate: {hits} hits, {misses} misses"
        );
    }
}

#[test]
fn rowid_alias_is_detected_exactly_as_sqlite_does() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.execute_batch(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, v);
         CREATE TABLE b (id INT PRIMARY KEY, v);
         CREATE TABLE c (id INTEGER PRIMARY KEY DESC, v);
         CREATE TABLE d (v, id INTEGER, PRIMARY KEY (id));
         CREATE TABLE e (\"we\"\"ird, col\" INTEGER PRIMARY KEY AUTOINCREMENT, [other col] TEXT);
         CREATE TABLE f (id integer primary key, v);",
    )
    .unwrap();
    for t in ["a", "b", "c", "d", "f"] {
        for i in [5, 1, 900, -3, 70_000] {
            conn.execute(&format!("INSERT INTO {t} (id, v) VALUES (?1, 'v{i}')"), [i])
                .unwrap();
        }
    }
    for i in [3, 1, 2] {
        conn.execute("INSERT INTO e VALUES (?1, 'x')", [i]).unwrap();
    }

    let mut db = Database::open(tmp.path()).unwrap();
    for t in ["a", "b", "c", "d", "e", "f"] {
        assert_table_matches(&mut db, &conn, t);
    }
}

#[test]
fn unreadable_tables_are_reported_not_misread() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.execute_batch(
        "CREATE TABLE ok (a);
         CREATE TABLE virt (a, b AS (a * 2));
         INSERT INTO ok VALUES (1);",
    )
    .unwrap();
    let mut db = Database::open(tmp.path()).unwrap();
    let virt = db.table("virt").unwrap();
    assert!(matches!(db.scan(&virt), Err(Error::Unsupported(_))));
    assert!(matches!(db.get(&virt, 1), Err(Error::Unsupported(_))));
    let ok = db.table("ok").unwrap();
    assert_eq!(db.scan(&ok).unwrap().count(), 1);
    assert!(matches!(db.table("missing"), Err(Error::NoSuchTable(_))));
}

#[test]
fn wal_mode_is_flagged() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))
        .unwrap();
    conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1);")
        .unwrap();
    drop(conn); // closing the last connection checkpoints the WAL into the main file
    assert!(Database::open(tmp.path()).unwrap().header().wal());
}

#[test]
fn non_databases_and_truncated_files_are_rejected() {
    assert!(matches!(
        Database::from_bytes(vec![]),
        Err(Error::NotSqlite)
    ));
    assert!(matches!(
        Database::from_bytes(vec![0; 4096]),
        Err(Error::NotSqlite)
    ));
    assert!(matches!(
        Database::from_bytes(b"SQLite format 3\0".to_vec()),
        Err(Error::NotSqlite)
    ));

    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.execute_batch("CREATE TABLE t (a); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 5000) INSERT INTO t SELECT i FROM n;").unwrap();
    drop(conn);
    let bytes = std::fs::read(tmp.path()).unwrap();
    assert!(Database::from_bytes(bytes.clone()).is_ok());
    assert!(matches!(
        Database::from_bytes(bytes[..bytes.len() / 2].to_vec()),
        Err(Error::Corrupt(_))
    ));
}

/// Corrupting a valid file must produce errors, never panics, hangs or huge allocations.
#[test]
fn random_corruption_never_panics() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.pragma_update(None, "page_size", 1024).unwrap();
    let mut rng = Rng(1234);
    populate(&conn, &mut rng, 250);
    drop(conn);
    let original = std::fs::read(tmp.path()).unwrap();

    let mut errors = 0u32;
    let mut clean = 0u32;
    for iteration in 0..6000u32 {
        let mut bytes = original.clone();
        for _ in 0..1 + rng.below(4) {
            let pos = if rng.chance(50) {
                rng.below(2048)
            } else {
                rng.below(bytes.len() as u64)
            } as usize;
            bytes[pos] = if rng.chance(30) {
                0xff
            } else {
                rng.next() as u8
            };
        }
        let outcome = std::panic::catch_unwind(|| {
            let Ok(mut db) = Database::from_bytes(bytes) else {
                return false;
            };
            let Ok(tables) = db.tables() else {
                return false;
            };
            let mut failed = false;
            for t in &tables {
                if t.unsupported.is_some() {
                    continue;
                }
                let mut seen = 0usize;
                if let Ok(scan) = db.scan(t) {
                    for row in scan {
                        seen += 1;
                        assert!(seen < 1_000_000, "scan does not terminate");
                        if row.is_err() {
                            failed = true;
                        }
                    }
                }
                let _ = db.get(t, (iteration % 700) as i64);
            }
            failed
        });
        match outcome {
            Ok(true) => errors += 1,
            Ok(false) => clean += 1,
            Err(_) => panic!("panic on corruption iteration {iteration}"),
        }
    }
    // Some corruptions land in unused bytes; most that matter must be detected.
    assert!(
        errors > 500,
        "only {errors} of 6000 corruptions were detected ({clean} clean)"
    );
}
