//! Storage formats beyond plain rowid tables: indexes, WITHOUT ROWID, UTF-16, generated
//! columns and ALTER TABLE defaults. Every result is compared with SQLite.

mod common;

use common::*;
use litewalk::{Database, Encoding, Error, Value};
use rusqlite::types::Value as SqlValue;

fn open(tmp: &TempDb) -> Database<std::fs::File> {
    Database::open(tmp.path()).unwrap()
}

fn scan_all(db: &mut Database<std::fs::File>, name: &str) -> Vec<Vec<Value>> {
    let table = db.table(name).unwrap();
    db.scan(&table)
        .unwrap()
        .map(|r| r.unwrap().values)
        .collect()
}

#[test]
fn without_rowid_tables_match_sqlite() {
    for page_size in [512u32, 4096, 65536] {
        let tmp = TempDb::new();
        let conn = tmp.connect();
        conn.pragma_update(None, "page_size", page_size).unwrap();
        conn.execute_batch(
            "CREATE TABLE w1 (k TEXT PRIMARY KEY, v, r REAL) WITHOUT ROWID;
             CREATE TABLE w2 (a, b INTEGER, c REAL, d TEXT, PRIMARY KEY (c, a)) WITHOUT ROWID;
             CREATE TABLE w3 (id INTEGER PRIMARY KEY, x, y) WITHOUT ROWID;
             CREATE TABLE w4 (a, b, PRIMARY KEY (b DESC, a)) WITHOUT ROWID;",
        )
        .unwrap();
        let mut rng = Rng(u64::from(page_size));
        conn.execute_batch("BEGIN").unwrap();
        for i in 0..800i64 {
            // Unique keys: the counter is part of every key.
            insert(
                &conn,
                "INSERT INTO w1 VALUES (?,?,?)",
                vec![
                    SqlValue::Text(format!("{}-{i}", gen_text(&mut rng))),
                    gen_any(&mut rng),
                    SqlValue::Real(gen_real(&mut rng)),
                ],
            );
            insert(
                &conn,
                "INSERT INTO w2 VALUES (?,?,?,?)",
                vec![
                    SqlValue::Integer(i),
                    SqlValue::Integer(gen_int(&mut rng)),
                    SqlValue::Real(gen_real(&mut rng) / 4.0),
                    SqlValue::Text(gen_text(&mut rng)),
                ],
            );
            insert(
                &conn,
                "INSERT INTO w3 VALUES (?,?,?)",
                vec![
                    SqlValue::Integer(i * 3 - 100),
                    gen_any(&mut rng),
                    gen_any(&mut rng),
                ],
            );
            insert(
                &conn,
                "INSERT INTO w4 VALUES (?,?)",
                vec![SqlValue::Integer(i), SqlValue::Integer(i % 17)],
            );
        }
        conn.execute_batch(
            "COMMIT; DELETE FROM w3 WHERE id % 5 = 0; DELETE FROM w4 WHERE a % 3 = 0;",
        )
        .unwrap();

        let mut db = open(&tmp);
        for (name, ncols) in [("w1", 3), ("w2", 4), ("w3", 3), ("w4", 2)] {
            let table = db.table(name).unwrap();
            assert!(table.without_rowid, "{name}");
            let expected = query_values(&conn, &format!("SELECT * FROM {name}"), ncols);
            let rows: Vec<_> = db.scan(&table).unwrap().collect::<Result<_, _>>().unwrap();
            assert!(rows.iter().all(|r| r.rowid.is_none()));
            let actual: Vec<_> = rows.into_iter().map(|r| r.values).collect();
            assert_same_rows(&actual, &expected, &format!("{name} page size {page_size}"));
        }
        let w1 = db.table("w1").unwrap();
        assert!(matches!(db.get(&w1, 1), Err(Error::Unsupported(_))));
    }
}

#[test]
fn utf16_databases_match_sqlite() {
    for (encoding, expected) in [
        ("UTF-16le", Encoding::Utf16Le),
        ("UTF-16be", Encoding::Utf16Be),
        ("UTF-8", Encoding::Utf8),
    ] {
        let tmp = TempDb::new();
        let conn = tmp.connect();
        conn.pragma_update(None, "encoding", encoding).unwrap();
        conn.pragma_update(None, "page_size", 1024).unwrap();
        // Non-ASCII table and column names: the schema text itself is stored in UTF-16.
        conn.execute_batch("CREATE TABLE \"tëst漢\" (id INTEGER PRIMARY KEY, t TEXT, b BLOB, n); CREATE INDEX ix ON \"tëst漢\"(t);").unwrap();
        let mut rng = Rng(99);
        for i in 0..400i64 {
            insert(
                &conn,
                "INSERT INTO \"tëst漢\" VALUES (?,?,?,?)",
                vec![
                    SqlValue::Integer(i),
                    SqlValue::Text(gen_text(&mut rng)),
                    SqlValue::Blob(gen_blob(&mut rng)),
                    gen_any(&mut rng),
                ],
            );
        }
        let mut db = open(&tmp);
        assert_eq!(db.header().encoding, expected);
        let table = db.table("tëst漢").unwrap();
        assert_eq!(table.column_names(), ["id", "t", "b", "n"]);
        let rows = scan_all(&mut db, "tëst漢");
        assert_same_rows(
            &rows,
            &query_values(&conn, "SELECT * FROM \"tëst漢\" ORDER BY id", 4),
            encoding,
        );

        let ix = db.index("ix").unwrap();
        let entries: Vec<_> = db
            .scan_index(&ix)
            .unwrap()
            .map(|e| e.unwrap().values)
            .collect();
        assert_same_rows(
            &entries,
            &query_values(
                &conn,
                "SELECT t, id FROM \"tëst漢\" INDEXED BY ix ORDER BY t, id",
                2,
            ),
            "utf-16 index",
        );
    }
}

#[test]
fn index_btrees_match_sqlite_for_every_page_size() {
    for (n, page_size) in [512u32, 1024, 4096, 65536].into_iter().enumerate() {
        let tmp = TempDb::new();
        let conn = tmp.connect();
        conn.pragma_update(None, "page_size", page_size).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a, b, c TEXT, d UNIQUE);
             CREATE INDEX t_a ON t(a);
             CREATE INDEX t_ab ON t(a, b DESC);
             CREATE UNIQUE INDEX t_c ON t(c, id);
             CREATE INDEX t_lower ON t(lower(c));
             CREATE INDEX t_partial ON t(b) WHERE a IS NOT NULL;",
        )
        .unwrap();
        let mut rng = Rng(5 + n as u64);
        conn.execute_batch("BEGIN").unwrap();
        for i in 0..1200i64 {
            insert(
                &conn,
                "INSERT INTO t VALUES (?,?,?,?,?)",
                vec![
                    SqlValue::Integer(i),
                    gen_any(&mut rng),
                    gen_any(&mut rng),
                    SqlValue::Text(gen_text(&mut rng)),
                    SqlValue::Integer(i * 7),
                ],
            );
        }
        conn.execute_batch("COMMIT; DELETE FROM t WHERE id % 4 = 0;")
            .unwrap();

        let mut db = open(&tmp);
        let cases: [(&str, &str, usize); 5] = [
            (
                "t_a",
                "SELECT a, id FROM t INDEXED BY t_a ORDER BY a, id",
                2,
            ),
            (
                "t_ab",
                "SELECT a, b, id FROM t INDEXED BY t_ab ORDER BY a, b DESC, id",
                3,
            ),
            (
                "t_c",
                "SELECT c, id, id FROM t INDEXED BY t_c ORDER BY c, id",
                3,
            ),
            (
                "t_lower",
                "SELECT lower(c), id FROM t INDEXED BY t_lower ORDER BY lower(c), id",
                2,
            ),
            (
                "t_partial",
                "SELECT b, id FROM t INDEXED BY t_partial WHERE a IS NOT NULL ORDER BY b, id",
                2,
            ),
        ];
        for (name, sql, ncols) in cases {
            let index = db.index(name).unwrap();
            let entries: Vec<_> = db.scan_index(&index).unwrap().map(|e| e.unwrap()).collect();
            let actual: Vec<_> = entries.iter().map(|e| e.values.clone()).collect();
            let expected = query_values(&conn, sql, ncols);
            // t_c stores (c, id, rowid), so `id` appears both as a key column and as the rowid.
            assert_same_rows(&actual, &expected, &format!("{name} @{page_size}"));
            assert!(entries.iter().all(|e| e.rowid().is_some()));
        }

        let indexes = db.indexes().unwrap();
        let by_name = |n: &str| indexes.iter().find(|i| i.name == n).unwrap().clone();
        assert!(by_name("t_c").unique && !by_name("t_a").unique);
        assert!(by_name("t_partial").partial);
        assert_eq!(
            by_name("t_ab")
                .columns
                .iter()
                .map(|c| (c.name.clone().unwrap(), c.descending))
                .collect::<Vec<_>>(),
            [("a".to_string(), false), ("b".to_string(), true)]
        );
        assert!(by_name("t_lower").columns[0].name.is_none());
        let auto = indexes
            .iter()
            .find(|i| i.automatic)
            .expect("UNIQUE column creates an automatic index");
        assert_eq!(auto.table, "t");
        let entries = db.scan_index(&auto.clone()).unwrap().count();
        assert_eq!(
            entries as i64,
            conn.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap()
        );
    }
}

#[test]
fn generated_columns() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.execute_batch(
        "CREATE TABLE s (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, c TEXT, d AS (a + 1) STORED);
         CREATE TABLE v (a INTEGER, b AS (a * 2) VIRTUAL);
         INSERT INTO s (a, c) VALUES (1, 'x'), (2, 'y'), (-7, NULL);
         INSERT INTO v (a) VALUES (1);",
    )
    .unwrap();
    let mut db = open(&tmp);
    let rows = scan_all(&mut db, "s");
    assert_same_rows(
        &rows,
        &query_values(&conn, "SELECT a, b, c, d FROM s ORDER BY rowid", 4),
        "stored generated columns",
    );
    let v = db.table("v").unwrap();
    assert_eq!(v.columns[1].generated, Some(litewalk::Generated::Virtual));
    assert!(matches!(db.scan(&v), Err(Error::Unsupported(_))));
}

#[test]
fn alter_table_add_column_defaults() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1), (2), (3);")
        .unwrap();
    let additions = [
        "i INTEGER DEFAULT 5",
        "j INTEGER DEFAULT -7",
        "r REAL DEFAULT 0",
        "r2 REAL DEFAULT 1.5",
        "n NUMERIC DEFAULT 2.0",
        "s TEXT DEFAULT 'it''s'",
        "u DEFAULT 'plain'",
        "z INTEGER DEFAULT NULL",
        "w INTEGER",
        "b BLOB DEFAULT X'00ff'",
        "t INTEGER DEFAULT TRUE",
        "m TEXT DEFAULT 42",
        "x INTEGER DEFAULT 'abc'",
    ];
    for col in additions {
        conn.execute_batch(&format!("ALTER TABLE t ADD COLUMN {col};"))
            .unwrap();
    }
    conn.execute_batch("INSERT INTO t (a, i) VALUES (4, 99);")
        .unwrap(); // a full-width row
    let mut db = open(&tmp);
    let rows = scan_all(&mut db, "t");
    assert_same_rows(
        &rows,
        &query_values(&conn, "SELECT * FROM t ORDER BY rowid", 1 + additions.len()),
        "defaults",
    );
}

#[test]
fn defaults_that_need_an_sql_engine_are_not_guessed() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1);")
        .unwrap();
    // SQLite converts the text '12' to the integer 12 under INTEGER affinity; litewalk declines.
    conn.execute_batch("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT '12';")
        .unwrap();
    let mut db = open(&tmp);
    let t = db.table("t").unwrap();
    let first = db.scan(&t).unwrap().next().unwrap();
    assert!(matches!(first, Err(Error::Unsupported(_))), "{first:?}");
}

#[test]
fn auto_vacuum_and_application_id_are_reported() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .unwrap();
    conn.pragma_update(None, "application_id", 0x1234_5678u32)
        .unwrap();
    conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1);")
        .unwrap();
    let db = open(&tmp);
    assert_eq!(db.header().auto_vacuum, litewalk::AutoVacuum::Incremental);
    assert_eq!(db.header().application_id, 0x1234_5678);
}

#[test]
fn borrowed_records_match_the_owned_decoder_on_stored_values() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.pragma_update(None, "page_size", 1024).unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a, b TEXT, c BLOB);")
        .unwrap();
    let mut rng = Rng(8);
    conn.execute_batch("BEGIN").unwrap();
    for i in 0..600i64 {
        insert(
            &conn,
            "INSERT INTO t VALUES (?,?,?,?)",
            vec![
                SqlValue::Integer(i),
                gen_any(&mut rng),
                SqlValue::Text(gen_text(&mut rng)),
                SqlValue::Blob(gen_blob(&mut rng)),
            ],
        );
    }
    conn.execute_batch("COMMIT").unwrap();
    let mut db = open(&tmp);
    let t = db.table("t").unwrap();
    let owned: Vec<_> = db.scan(&t).unwrap().map(|r| r.unwrap()).collect();
    let mut borrowed = Vec::new();
    let n = db
        .for_each_record(&t, |rowid, values| {
            borrowed.push((
                rowid,
                values
                    .iter()
                    .map(litewalk::ValueRef::to_owned)
                    .collect::<Vec<_>>(),
            ));
            true
        })
        .unwrap();
    assert_eq!(n as usize, owned.len());
    for (o, (rowid, vals)) in owned.iter().zip(&borrowed) {
        assert_eq!(o.rowid, *rowid);
        // The stored record holds NULL for the INTEGER PRIMARY KEY column; every other column is identical.
        assert_eq!(vals[0], Value::Null);
        assert!(o.values[1..]
            .iter()
            .zip(&vals[1..])
            .all(|(x, y)| same(x, y)));
    }
    let mut seen = 0;
    db.for_each_record(&t, |_, _| {
        seen += 1;
        seen < 10
    })
    .unwrap();
    assert_eq!(seen, 10, "returning false stops the visit");
}

#[test]
fn borrowed_records_refuse_utf16() {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.pragma_update(None, "encoding", "UTF-16le").unwrap();
    conn.execute_batch("CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('x');")
        .unwrap();
    let mut db = open(&tmp);
    let t = db.table("t").unwrap();
    assert!(matches!(
        db.for_each_record(&t, |_, _| true),
        Err(Error::Unsupported(_))
    ));
}
