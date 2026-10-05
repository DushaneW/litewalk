//! Runs the real binary against databases written by SQLite.

mod common;

use common::corpus::base_db;
use common::*;
use rusqlite::types::Value as SqlValue;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_litewalk"))
        .args(args)
        .output()
        .expect("binary runs")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("utf-8 stdout")
}

/// Value of a `key:  value` line.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key).map(str::trim))
}

fn fixture() -> TempDb {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.pragma_update(None, "page_size", 1024).unwrap();
    conn.pragma_update(None, "user_version", 7).unwrap();
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT DEFAULT 'n/a', blob BLOB, score REAL);
         CREATE INDEX idx_users_name ON users(name);
         CREATE VIEW v AS SELECT name FROM users;
         CREATE TRIGGER tr AFTER INSERT ON users BEGIN SELECT 1; END;
         CREATE TABLE w (k TEXT PRIMARY KEY, v) WITHOUT ROWID;",
    )
    .unwrap();
    insert(
        &conn,
        "INSERT INTO users VALUES (1, 'ada', 'line1\nline2\t\"q\"', X'00ff10', 1.5)",
        vec![],
    );
    insert(
        &conn,
        "INSERT INTO users VALUES (2, 'bob, jr.', '', NULL, NULL)",
        vec![],
    );
    for i in 3..300 {
        insert(
            &conn,
            "INSERT INTO users VALUES (?, ?, ?, NULL, NULL)",
            vec![
                SqlValue::Integer(i),
                SqlValue::Text(format!("user{i}")),
                SqlValue::Text("x".repeat(120)),
            ],
        );
    }
    insert(
        &conn,
        "INSERT INTO users VALUES (1000, 'big', ?, NULL, NULL)",
        vec![SqlValue::Text("B".repeat(5000))],
    );
    insert(&conn, "INSERT INTO w VALUES ('a', 1), ('b', 2)", vec![]);
    tmp
}

#[test]
fn json_output_is_valid_json_for_every_command() {
    let tmp = fixture();
    let db = tmp.path().to_str().unwrap();
    // The test crate has no JSON parser (and production code has none): validate with Python's.
    let commands: Vec<Vec<&str>> = vec![
        vec!["info", db, "--json"],
        vec!["tables", db, "--json"],
        vec!["indexes", db, "--json"],
        vec!["objects", db, "--json"],
        vec!["schema", db, "--json", "--pretty"],
        vec!["page", db, "2", "--json"],
        vec!["page", db, "2", "--json", "--hex"],
        vec!["check", db, "--json"],
        vec!["map", db, "--json"],
        vec!["stats", db, "--json"],
        vec!["forensic", db, "--json", "--free-space"],
        vec![
            "dump", db, "users", "--format", "json", "--rowid", "--limit", "5",
        ],
        vec![
            "dump", db, "users", "--format", "json", "--pretty", "--limit", "3",
        ],
        vec!["get", db, "users", "1", "--json"],
    ];
    for argv in commands {
        let o = run(&argv);
        assert!(
            o.status.success(),
            "{argv:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let py = Command::new("python3")
            .args(["-c", "import json,sys; json.load(sys.stdin)"])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = py else { return }; // no python3: skip validation
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), &o.stdout).unwrap();
        let res = child.wait_with_output().unwrap();
        assert!(
            res.status.success(),
            "{argv:?} produced invalid JSON: {}\n{}",
            String::from_utf8_lossy(&res.stderr),
            stdout(&o)
        );
    }
}

#[test]
fn info_tables_indexes_objects_and_schema() {
    let tmp = fixture();
    let db = tmp.path().to_str().unwrap();
    let info = stdout(&run(&["info", db]));
    assert_eq!(field(&info, "page size:"), Some("1024"));
    assert_eq!(field(&info, "user version:"), Some("7"));
    assert_eq!(field(&info, "encoding:"), Some("UTF-8"));
    assert_eq!(field(&info, "tables:"), Some("2"));
    assert_eq!(field(&info, "indexes:"), Some("1"));
    assert_eq!(field(&info, "journal mode:"), Some("rollback"));
    assert_eq!(field(&info, "WAL file:"), Some("absent"));

    let tables = stdout(&run(&["tables", db]));
    assert!(
        tables.contains("users\tid, name, note, blob, score"),
        "{tables}"
    );

    let indexes = stdout(&run(&["indexes", db]));
    assert!(
        indexes.contains("idx_users_name") && indexes.contains("users") && indexes.contains("name"),
        "{indexes}"
    );

    let objects = stdout(&run(&["objects", db]));
    for line in [
        "TABLE     users",
        "INDEX     idx_users_name",
        "VIEW      v",
        "TRIGGER   tr",
    ] {
        assert!(objects.contains(line), "missing `{line}` in\n{objects}");
    }

    let schema = stdout(&run(&["schema", db]));
    assert!(
        schema.contains("TABLE users")
            && schema.contains("PRIMARY KEY")
            && schema.contains("NOT NULL")
            && schema.contains("DEFAULT 'n/a'"),
        "{schema}"
    );
    assert!(
        schema.contains("WITHOUT ROWID")
            && schema.contains("INDEX idx_users_name ON users (name)")
            && schema.contains("VIEW v")
            && schema.contains("TRIGGER tr"),
        "{schema}"
    );
}

#[test]
fn dump_formats_and_options() {
    let tmp = fixture();
    let db = tmp.path().to_str().unwrap();

    let csv = stdout(&run(&[
        "dump",
        db,
        "users",
        "--format",
        "csv",
        "--limit",
        "2",
        "--hex-blobs",
    ]));
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], "id,name,note,blob,score");
    assert!(csv.contains("\"line1\nline2\t\"\"q\"\"\""), "{csv}"); // RFC 4180 quoting
    assert!(
        csv.contains("1,ada,") && csv.contains("x'00ff10'") && csv.contains("\"bob, jr.\",\"\",,"),
        "{csv}"
    ); // empty text quoted, NULL empty

    let tsv = stdout(&run(&["dump", db, "users", "--limit", "1", "--rowid"]));
    assert!(tsv.starts_with("rowid\tid\tname\tnote\tblob\tscore\n1\t1\tada\tline1\\nline2\\t\"q\"\t<blob 3 bytes>\t1.5\n"), "{tsv}");

    let picked = stdout(&run(&[
        "dump",
        db,
        "users",
        "--columns",
        "name,id",
        "--offset",
        "1",
        "--limit",
        "2",
    ]));
    assert_eq!(picked, "name\tid\nbob, jr.\t2\nuser3\t3\n");

    let json = stdout(&run(&[
        "dump", db, "users", "--format", "json", "--limit", "2", "--rowid",
    ]));
    assert!(json.starts_with("[{\"rowid\":1,\"id\":1,\"name\":\"ada\",\"note\":\"line1\\nline2\\t\\\"q\\\"\",\"blob\":{\"blob\":\"00ff10\"},\"score\":1.5},"), "{json}");
    assert!(json.contains("\"blob\":null"));

    let w = stdout(&run(&["dump", db, "w"]));
    assert_eq!(w, "k\tv\na\t1\nb\t2\n");

    let get = stdout(&run(&["get", db, "users", "2"]));
    assert!(
        get.contains("name\tbob, jr.") && get.contains("score\tNULL"),
        "{get}"
    );
}

#[test]
fn usage_errors_use_exit_status_2_and_stderr() {
    let tmp = fixture();
    let db = tmp.path().to_str().unwrap();
    for argv in [
        vec![],
        vec!["frobnicate", db],
        vec!["info"],
        vec!["dump", db],
        vec!["dump", db, "users", "--columns", "nope"],
        vec!["page", db, "x"],
        vec!["info", db, "--bogus"],
        vec!["dump", db, "users", "--limit", "-1"],
    ] {
        let o = run(&argv);
        assert_eq!(o.status.code(), Some(2), "{argv:?}");
        assert!(o.stdout.is_empty(), "{argv:?} wrote to stdout");
        assert!(!o.stderr.is_empty());
    }
}

#[test]
fn operational_errors_use_exit_status_1() {
    let tmp = fixture();
    let db = tmp.path().to_str().unwrap();
    for argv in [
        vec!["dump", db, "missing"],
        vec!["page", db, "99999"],
        vec!["get", db, "users", "424242"],
        vec!["info", "/nonexistent/file.db"],
        vec!["wal", db],
    ] {
        let o = run(&argv);
        assert_eq!(
            o.status.code(),
            Some(1),
            "{argv:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(o.stdout.is_empty(), "{argv:?}");
    }
    let garbage = TempDb::new();
    std::fs::write(garbage.path(), vec![7u8; 5000]).unwrap();
    let o = run(&["tables", garbage.path().to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("not a SQLite database"));
}

#[test]
fn check_reports_sections_and_exit_status() {
    let tmp = fixture();
    let ok = run(&["check", tmp.path().to_str().unwrap()]);
    assert!(ok.status.success());
    let text = stdout(&ok);
    for section in [
        "✓ SQLite header",
        "✓ page boundaries",
        "✓ page types",
        "✓ cell pointers",
        "✓ B-tree structure",
        "✓ B-tree ordering",
        "✓ overflow chains",
        "✓ schema",
        "✓ record payloads",
    ] {
        assert!(text.contains(section), "missing `{section}`:\n{text}");
    }
    assert!(text.contains("Database is structurally valid."));

    // corrupt an overflow chain
    let (mut bytes, layout) = base_db();
    let start = *layout
        .overflow_chain
        .iter()
        .find(|&&p| {
            !layout.overflow_chain.iter().any(|&q| {
                u32::from_be_bytes(
                    common::corpus::page_bytes(&bytes, q)[..4]
                        .try_into()
                        .unwrap(),
                ) == p
            })
        })
        .unwrap();
    common::corpus::patch(&mut bytes, start, 0, &999_999u32.to_be_bytes());
    let bad = TempDb::new();
    std::fs::write(bad.path(), &bytes).unwrap();
    let o = run(&["check", bad.path().to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    let text = stdout(&o);
    assert!(
        text.contains("✗ overflow chains")
            && text.contains("999999")
            && text.contains("structural problem"),
        "{text}"
    );
    let json = stdout(&run(&["check", bad.path().to_str().unwrap(), "--json"]));
    assert!(json.starts_with("{\"valid\":false"), "{json}");

    // not a database at all: reported as a header failure, still exit 1
    let junk = TempDb::new();
    std::fs::write(junk.path(), b"hello").unwrap();
    let o = run(&["check", junk.path().to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stdout(&o).contains("✗ SQLite header"));
}

#[test]
fn page_map_and_stats_output() {
    let tmp = fixture();
    let db = tmp.path().to_str().unwrap();

    let map = stdout(&run(&["map", db]));
    assert!(map.contains("1      [HEADER + SQLITE_SCHEMA]"), "{map}");
    assert!(
        map.contains("[TABLE] users")
            && map.contains("[INDEX] idx_users_name")
            && map.contains("[OVERFLOW] users"),
        "{map}"
    );

    let page = stdout(&run(&["page", db, "1"]));
    assert!(page.starts_with("PAGE 1\n────"), "{page}");
    let interior = stdout(&run(&["page", db, "2"]));
    assert_eq!(
        field(&interior, "type:"),
        Some("table interior"),
        "{interior}"
    );
    assert!(
        interior.contains("left child:  page 5")
            && interior.contains("max rowid:")
            && interior.contains("rightmost:"),
        "{interior}"
    );
    let leaf = stdout(&run(&["page", db, "5"]));
    assert_eq!(field(&leaf, "type:"), Some("table leaf"), "{leaf}");
    assert!(
        leaf.contains("Cell 0")
            && leaf.contains("rowid:")
            && leaf.contains("overflow:    no")
            && leaf.contains("payload:"),
        "{leaf}"
    );

    let hex = stdout(&run(&["page", db, "1", "--hex"]));
    assert!(
        hex.contains("REGIONS")
            && hex.contains("database header")
            && hex.contains("cell pointer array")
            && hex.contains("000000  53 51 4c 69 74 65"),
        "{hex}"
    );

    let stats = stdout(&run(&["stats", db, "--cache", "8"]));
    assert!(
        stats.contains("Cache capacity: 8 pages")
            && stats.contains("Pages read:")
            && stats.contains("Hit ratio:"),
        "{stats}"
    );
    let uncached = stdout(&run(&["stats", db, "--cache", "0"]));
    assert!(
        uncached.contains("Cache hits:     0") && uncached.contains("Hit ratio:      0.00%"),
        "{uncached}"
    );
}

#[test]
fn wal_and_forensic_commands() {
    let live = TempDb::new();
    let conn = live.connect();
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))
        .unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);")
        .unwrap();
    let snap = TempDb::new();
    std::fs::copy(live.path(), snap.path()).unwrap();
    let mut wal = snap.path().as_os_str().to_owned();
    wal.push("-wal");
    let mut live_wal = live.path().as_os_str().to_owned();
    live_wal.push("-wal");
    std::fs::copy(live_wal, &wal).unwrap();

    let db = snap.path().to_str().unwrap();
    let out = stdout(&run(&["wal", db]));
    assert!(
        out.contains("frames:") && out.contains("committed:") && out.contains("page size:    4096"),
        "{out}"
    );
    let verbose = stdout(&run(&["wal", db, "-v"]));
    assert!(
        verbose.contains("frame  page  commit") && verbose.contains("commit, db size"),
        "{verbose}"
    );

    let info = stdout(&run(&["info", db]));
    assert_eq!(field(&info, "journal mode:"), Some("WAL"));
    assert!(
        field(&info, "WAL file:")
            .is_some_and(|v| v.starts_with("present") && v.contains("not applied")),
        "{info}"
    );
    let applied = stdout(&run(&["dump", db, "t", "--with-wal"]));
    assert_eq!(applied, "a\n1\n2\n");

    let f = stdout(&run(&["forensic", db, "--with-wal", "--free-space"]));
    assert!(
        f.contains("FORENSIC REPORT")
            && f.contains("nothing is recovered")
            && f.contains("EXPERIMENTAL"),
        "{f}"
    );
    assert_eq!(field(&f, "anomalies:"), Some("none"), "{f}");
    assert_eq!(field(&f, "tables:"), Some("1"));
    assert!(
        field(&f, "WAL file:").is_some_and(|v| v.contains("applied") && !v.contains("not applied")),
        "{f}"
    );
    drop(conn);
}

#[test]
fn help_and_version() {
    let h = run(&["--help"]);
    assert!(h.status.success() && stdout(&h).contains("usage: litewalk"));
    let v = run(&["--version"]);
    assert!(stdout(&v).starts_with("litewalk 0."));
}
