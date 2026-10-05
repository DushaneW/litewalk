mod common;

use common::corpus::{base_db, cases, schema_cases};
use common::*;
use litewalk::{check_reader, Database, Usage, SECTIONS};
use rusqlite::types::Value as SqlValue;
use std::io::Cursor;

/// Reads everything the API offers; the only acceptable outcomes are values and errors.
fn exercise(bytes: Vec<u8>) {
    let Ok(mut db) = Database::from_bytes(bytes) else {
        return;
    };
    if let Ok(tables) = db.tables() {
        for t in &tables {
            if let Ok(scan) = db.scan(t) {
                for (n, row) in scan.enumerate() {
                    assert!(n < 1_000_000, "scan does not terminate");
                    if row.is_err() {
                        break;
                    }
                }
            }
            let _ = db.get(t, 5);
            let _ = db.get(t, i64::MAX);
        }
    }
    if let Ok(indexes) = db.indexes() {
        for i in &indexes {
            if let Ok(scan) = db.scan_index(i) {
                for (n, e) in scan.enumerate() {
                    assert!(n < 1_000_000, "index scan does not terminate");
                    if e.is_err() {
                        break;
                    }
                }
            }
        }
    }
    for p in 1..=db.header().page_count.min(300) {
        let _ = db.page(p);
    }
    let _ = db.check();
    let _ = db.map();
    let _ = db.freelist();
}

fn assert_valid(bytes: Vec<u8>, what: &str) {
    let report = check_reader(Cursor::new(bytes.clone())).unwrap();
    assert!(
        report.is_valid(),
        "{what}: {:#?}",
        report.issues.iter().take(8).collect::<Vec<_>>()
    );
    let mut db = Database::from_bytes(bytes).unwrap();
    let map = db.map().unwrap();
    assert!(
        map.iter().all(|e| e.usage != Usage::Unreferenced),
        "{what}: every page of a valid database has an owner"
    );
}

#[test]
fn databases_written_by_sqlite_pass_the_check() {
    // Mixed workloads at every page size, including deletes and updates that leave free
    // blocks, fragmented pages and a freelist.
    for (n, page_size) in [512u32, 1024, 2048, 4096, 8192, 16384, 32768, 65536]
        .into_iter()
        .enumerate()
    {
        let tmp = TempDb::new();
        let conn = tmp.connect();
        conn.pragma_update(None, "page_size", page_size).unwrap();
        conn.execute_batch(
            "CREATE TABLE t1 (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d BLOB, e);
             CREATE TABLE t2 (x, y);
             CREATE TABLE t3 (z);
             CREATE TABLE w (k TEXT PRIMARY KEY, v) WITHOUT ROWID;
             CREATE INDEX t1_a ON t1(a);
             CREATE INDEX t1_c ON t1(c);
             CREATE INDEX t2_xy ON t2(x, y);",
        )
        .unwrap();
        let mut rng = Rng(77 + n as u64);
        conn.execute_batch("BEGIN").unwrap();
        for i in 0..1200i64 {
            insert(
                &conn,
                "INSERT INTO t1 VALUES (?,?,?,?,?,?)",
                vec![
                    SqlValue::Integer(i * 2),
                    SqlValue::Integer(gen_int(&mut rng)),
                    SqlValue::Real(gen_real(&mut rng)),
                    SqlValue::Text(gen_text(&mut rng)),
                    SqlValue::Blob(gen_blob(&mut rng)),
                    gen_any(&mut rng),
                ],
            );
            insert(
                &conn,
                "INSERT INTO t2 VALUES (?,?)",
                vec![gen_any(&mut rng), gen_any(&mut rng)],
            );
            insert(
                &conn,
                "INSERT INTO w VALUES (?,?)",
                vec![
                    SqlValue::Text(format!("{i}-{}", gen_text(&mut rng))),
                    gen_any(&mut rng),
                ],
            );
        }
        conn.execute_batch("COMMIT; DELETE FROM t1 WHERE id % 3 = 0; DELETE FROM t2 WHERE rowid % 2 = 0; DELETE FROM w WHERE k < '3';").unwrap();
        conn.execute(
            "UPDATE t1 SET c = ?1 WHERE id % 7 = 0",
            [gen_text(&mut rng)],
        )
        .unwrap();
        drop(conn);
        assert_valid(
            std::fs::read(tmp.path()).unwrap(),
            &format!("page size {page_size}"),
        );
    }
}

#[test]
fn special_database_shapes_pass_the_check() {
    let build = |setup: &str| -> Vec<u8> {
        let tmp = TempDb::new();
        let conn = tmp.connect();
        conn.execute_batch(setup).unwrap();
        drop(conn);
        std::fs::read(tmp.path()).unwrap()
    };
    let filler = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 3000) INSERT INTO t SELECT i, printf('%0200d', i) FROM n;";
    let cases = [
        ("one empty table", "CREATE TABLE t (a);".to_owned()),
        ("auto_vacuum full", format!("PRAGMA auto_vacuum=FULL; PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, v); {filler} DELETE FROM t WHERE id % 2 = 0;")),
        ("auto_vacuum incremental", format!("PRAGMA auto_vacuum=INCREMENTAL; PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, v); {filler} DELETE FROM t;")),
        ("vacuumed", format!("CREATE TABLE t (id INTEGER PRIMARY KEY, v); {filler} DELETE FROM t WHERE id > 100; VACUUM;")),
        ("UTF-16le", "PRAGMA encoding='UTF-16le'; CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('héllo'); CREATE INDEX i ON t(a);".to_owned()),
        ("page 1 interior root", format!("PRAGMA page_size=512; {}", (0..30).map(|i| format!("CREATE TABLE table_with_a_long_name_{i} (column_one INTEGER, column_two TEXT, column_three BLOB);")).collect::<Vec<_>>().join(" "))),
        ("views and triggers", "CREATE TABLE t (a); CREATE VIEW v AS SELECT a FROM t; CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END;".to_owned()),
        ("autoincrement", "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v); INSERT INTO t (v) VALUES (1),(2);".to_owned()),
    ];
    for (name, sql) in cases {
        assert_valid(build(&sql), name);
    }
}

#[test]
fn deterministic_corruptions_are_caught_by_the_expected_check() {
    let mut all = cases();
    all.extend(schema_cases());
    assert!(all.len() >= 36, "corpus shrank to {} cases", all.len());
    for case in all {
        let report = check_reader(Cursor::new(case.bytes.clone())).unwrap();
        assert!(
            !report.is_valid(),
            "{}: corruption was not detected",
            case.name
        );
        assert!(
            !report.issues_in(case.section).is_empty(),
            "{}: expected an issue in `{}`, got {:#?}",
            case.name,
            case.section.name(),
            report.issues
        );
        exercise(case.bytes); // reading a corrupt file must not panic or hang
    }
}

#[test]
fn a_valid_base_database_has_no_issues_and_every_section_is_checked() {
    let (bytes, _) = base_db();
    let report = check_reader(Cursor::new(bytes)).unwrap();
    assert!(report.is_valid(), "{:#?}", report.issues);
    assert!(report.skipped.is_empty());
    assert_eq!(SECTIONS.len(), 11);
    assert!(report.rows >= 120);
    assert_eq!(report.tables, 2);
    assert_eq!(report.indexes, 1);
}

#[test]
fn errors_carry_location_context() {
    let all = cases();
    let get = |n: &str| all.iter().find(|c| c.name == n).unwrap().bytes.clone();

    let mut db = Database::from_bytes(get("overflow_points_to_itself")).unwrap();
    let t = db.table("t").unwrap();
    let err = db
        .scan(&t)
        .unwrap()
        .find_map(|r| r.err())
        .expect("scan reaches the broken chain");
    let c = err.corruption().expect("is a corruption");
    assert_eq!(c.kind, litewalk::CorruptionKind::Overflow);
    assert!(c.reason.contains("points to itself"), "{c}");
    assert!(
        c.page.is_some()
            && c.table.as_deref() == Some("t")
            && c.rowid.is_some()
            && c.cell.is_some(),
        "{c:?}"
    );

    let mut db = Database::from_bytes(get("interior_child_out_of_range")).unwrap();
    let t = db.table("t").unwrap();
    let err = db.scan(&t).unwrap().find_map(|r| r.err()).unwrap();
    let c = err.corruption().unwrap();
    assert_eq!(c.kind, litewalk::CorruptionKind::PageNumber);
    assert_eq!(
        (c.expected.is_some(), c.actual.as_deref()),
        (true, Some("999999"))
    );
    assert!(c.depth.is_some() && c.table.as_deref() == Some("t"));
    assert!(err.to_string().contains("999999"));
}

#[test]
fn page_inspection_matches_the_page_map() {
    let (bytes, layout) = base_db();
    let mut db = Database::from_bytes(bytes).unwrap();
    let leaf = db.page(layout.first_leaf).unwrap();
    assert_eq!(leaf.page_type, Some(litewalk::PageType::LeafTable));
    assert_eq!(leaf.cell_count, leaf.cells.len());
    assert!(leaf
        .cells
        .iter()
        .all(|c| c.rowid.is_some() && c.error.is_none()));
    assert!(leaf.problems.is_empty(), "{:?}", leaf.problems);
    let spans: usize = leaf.cells.iter().map(|c| c.size.max(4)).sum::<usize>()
        + leaf.freeblocks.iter().map(|b| b.size).sum::<usize>();
    // header(8) + pointers + free space + cells accounts for the whole page, within SQLite's min-cell padding
    assert_eq!(
        8 + 2 * leaf.cell_count + leaf.free_bytes + spans,
        leaf.usable
    );

    let interior = db.page(layout.interior).unwrap();
    assert_eq!(interior.page_type, Some(litewalk::PageType::InteriorTable));
    assert!(interior.rightmost.is_some() && interior.cells.iter().all(|c| c.left_child.is_some()));

    let overflow = db.page(layout.overflow_chain[0]).unwrap();
    assert_eq!(overflow.page_type, None);
    assert!(db.page(0).is_err() && db.page(10_000).is_err());
    let with_overflow = (1..=db.header().page_count)
        .filter_map(|p| db.page(p).ok())
        .any(|p| p.cells.iter().any(|c| c.overflow_page.is_some()));
    assert!(with_overflow);
}

#[test]
fn freelist_walk_matches_sqlite() {
    let (bytes, layout) = base_db();
    let mut db = Database::from_bytes(bytes).unwrap();
    let fl = db.freelist().unwrap();
    assert_eq!(fl.trunks.first().copied(), Some(layout.freelist_trunk));
    assert_eq!(
        (fl.trunks.len() + fl.leaves.len()) as u32,
        db.header().freelist_count
    );
}
